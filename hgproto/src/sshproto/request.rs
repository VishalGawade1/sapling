// Copyright (c) 2004-present, Facebook, Inc.
// All Rights Reserved.
//
// This software may be used and distributed according to the terms of the
// GNU General Public License version 2 or any later version.

use percent_encoding::percent_decode;
use std::collections::{HashMap, HashSet};
use std::iter;
use std::iter::FromIterator;
use std::str::{self, FromStr};

use bytes::{Bytes, BytesMut};
use nom::{is_alphanumeric, is_digit, Err, ErrorKind, FindSubstring, IResult, Needed, Slice};

use HgNodeHash;

use batch;
use errors;
use errors::*;
use {GetbundleArgs, GettreepackArgs, Request, SingleRequest};

const BAD_UTF8_ERR_CODE: u32 = 111;

/// Parse an unsigned decimal integer. If it reaches the end of input, it returns Incomplete,
/// as there may be more digits following
fn digit<F: Fn(u8) -> bool>(input: &[u8], isdigit: F) -> IResult<&[u8], &[u8]> {
    for (idx, item) in input.iter().enumerate() {
        if !isdigit(*item) {
            if idx == 0 {
                return IResult::Error(Err::Code(ErrorKind::Digit));
            } else {
                return IResult::Done(&input[idx..], &input[0..idx]);
            }
        }
    }
    IResult::Incomplete(Needed::Unknown)
}

named!(
    integer<usize>,
    map_res!(
        map_res!(apply!(digit, is_digit), str::from_utf8),
        FromStr::from_str
    )
);

/// Return an identifier of the form [a-zA-Z_][a-zA-Z0-9_]*. Returns Incomplete
/// if it manages to reach the end of input, as there may be more identifier coming.
fn ident_alphanum(input: &[u8]) -> IResult<&[u8], &[u8]> {
    for (idx, item) in input.iter().enumerate() {
        match *item as char {
            'a'...'z' | 'A'...'Z' | '_' => continue,
            '0'...'9' if idx > 0 => continue,
            _ => {
                if idx > 0 {
                    return IResult::Done(&input[idx..], &input[0..idx]);
                } else {
                    return IResult::Error(Err::Code(ErrorKind::AlphaNumeric));
                }
            }
        }
    }
    IResult::Incomplete(Needed::Unknown)
}

/// Return an identifier of the form [a-zA-Z0-9_-%]*.
named!(
    ident,
    take_till1!(|ch| match ch as char {
        '0'...'9' | 'a'...'z' | 'A'...'Z' | '_' | '-' | '%' => false,
        _ => true,
    })
);

/// As above, but assumes input is complete, so reaching the end of input means
/// the identifier is the entire input.
fn ident_complete<P>(parser: P) -> impl Fn(&[u8]) -> IResult<&[u8], &[u8]>
where
    P: Fn(&[u8]) -> IResult<&[u8], &[u8]>,
{
    move |input| match parser(input) {
        IResult::Incomplete(_) => IResult::Done(&b""[..], input),
        other => other,
    }
}

/// Assumption: input is complete
/// We can't use 'integer' defined above as it reads until a non digit character
named!(
    boolean<bool>,
    map_res!(take_while1!(is_digit), |s| -> Result<bool> {
        let s = str::from_utf8(s)?;
        Ok(u32::from_str(s)? != 0)
    })
);

named!(
    batch_param_comma_separated<Bytes>,
    map_res!(
        do_parse!(key: take_while!(notcomma) >> (key)),
        |k: &[u8]| if k.is_empty() {
            bail_msg!("empty input while parsing batch params")
        } else {
            Ok::<_, Error>(Bytes::from(batch::unescape(k)?))
        }
    )
);

/// A "*" parameter is a meta-parameter - its argument is a count of
/// a number of other parameters. (We accept nested/recursive star parameters,
/// but I don't know if that ever happens in practice.)
named!(
    param_star<HashMap<Vec<u8>, Vec<u8>>>,
    do_parse!(tag!(b"* ") >> count: integer >> tag!(b"\n") >> res: apply!(params, count) >> (res))
);

/// List of comma-separated values, each of which is encoded using batch param encoding.
named!(
    gettreepack_directories<Vec<Bytes>>,
    separated_list_complete!(tag!(","), batch_param_comma_separated)
);

/// A named parameter is a name followed by a decimal integer of the number of
/// bytes in the parameter, followed by newline. The parameter value has no terminator.
/// ident <bytelen>\n
/// <bytelen bytes>
named!(
    param_kv<HashMap<Vec<u8>, Vec<u8>>>,
    do_parse!(
        key: ident_alphanum
            >> tag!(b" ")
            >> len: integer
            >> tag!(b"\n")
            >> val: take!(len)
            >> (iter::once((key.to_vec(), val.to_vec())).collect())
    )
);

/// Normal ssh protocol params:
/// either a "*", which indicates a number of following parameters,
/// or a named parameter whose value bytes follow.
/// "count" is the number of required parameters, including the "*" parameter - but *not*
/// the parameters that the "*" parameter expands to.
fn params(inp: &[u8], count: usize) -> IResult<&[u8], HashMap<Vec<u8>, Vec<u8>>> {
    let mut inp = inp;
    let mut have = 0;

    let mut ret = HashMap::with_capacity(count);

    while have < count {
        let res = alt!(inp,
              param_star => { |kv: HashMap<_, _>| { have += 1; kv } }
            | param_kv => { |kv: HashMap<_, _>| { have += kv.len(); kv } }
        );

        match res {
            IResult::Done(rest, val) => {
                for (k, v) in val.into_iter() {
                    ret.insert(k, v);
                }
                inp = rest;
            }
            failed => return failed,
        }
    }

    IResult::Done(inp, ret)
}

fn notcomma(b: u8) -> bool {
    b != b','
}

/// A batch parameter is "name=value", where name ad value are escaped with an ad-hoc
/// scheme to protect ',', ';', '=', ':'. The value ends either at the end of the input
/// (which is actually from the "batch" command "cmds" parameter), or at a ',', as they're
/// comma-delimited.
named!(
    batch_param_escaped<(Vec<u8>, Vec<u8>)>,
    map_res!(
        do_parse!(key: take_until_and_consume1!("=") >> val: take_while!(notcomma) >> ((key, val))),
        |(k, v)| Ok::<_, Error>((batch::unescape(k)?, batch::unescape(v)?))
    )
);

/// Extract parameters from batch - same signature as params
/// Batch parameters are a comma-delimited list of parameters; count is unused
/// and there's no notion of star params.
named_args!(batch_params(_count: usize)<HashMap<Vec<u8>, Vec<u8>>>,
    map!(
        separated_list_complete!(tag!(","), batch_param_escaped),
        |v: Vec<_>| v.into_iter().collect()
    )
);

/// A nodehash is simply 40 hex digits.
named!(
    nodehash<HgNodeHash>,
    map_res!(take!(40), |v: &[u8]| str::parse(str::from_utf8(v)?))
);

/// A pair of nodehashes, separated by '-'
named!(
    pair<(HgNodeHash, HgNodeHash)>,
    do_parse!(a: nodehash >> tag!("-") >> b: nodehash >> ((a, b)))
);

/// A space-separated list of pairs.
named!(
    pairlist<Vec<(HgNodeHash, HgNodeHash)>>,
    separated_list_complete!(tag!(" "), pair)
);

/// A space-separated list of node hashes
named!(
    hashlist<Vec<HgNodeHash>>,
    separated_list_complete!(tag!(" "), nodehash)
);

/// A space-separated list of strings
named!(
    stringlist<Vec<String>>,
    separated_list!(
        complete!(tag!(" ")),
        map_res!(
            map_res!(take_while!(is_alphanumeric), str::from_utf8),
            FromStr::from_str
        )
    )
);

/// A comma-separated list of arbitrary values. The input is assumed to be
/// complete and exact.
fn commavalues(input: &[u8]) -> IResult<&[u8], Vec<Vec<u8>>> {
    if input.len() == 0 {
        // Need to handle this separately because the below will return
        // vec![vec![]] on an empty input.
        IResult::Done(b"", vec![])
    } else {
        IResult::Done(
            b"",
            input
                .split(|c| *c == b',')
                .map(|val| val.to_vec())
                .collect(),
        )
    }
}

fn notsemi(b: u8) -> bool {
    b != b';'
}

/// A command in a batch. Commands are represented as "command parameters". The parameters
/// end either at the end of the buffer or at ';'.
named!(
    cmd<(Vec<u8>, Vec<u8>)>,
    do_parse!(
        cmd: take_until_and_consume1!(" ")
            >> args: take_while!(notsemi)
            >> ((cmd.to_vec(), args.to_vec()))
    )
);

/// A list of batched commands - the list is delimited by ';'.
named!(
    cmdlist<Vec<(Vec<u8>, Vec<u8>)>>,
    separated_list!(complete!(tag!(";")), cmd)
);

named!(match_eof<&'a [u8]>, eof!());
/// Given a hash of parameters, look up a parameter by name, and if it exists,
/// apply a parser to its value. If it doesn't, error out.
fn parseval<'a, F, T>(params: &'a HashMap<Vec<u8>, Vec<u8>>, key: &str, parser: F) -> Result<T>
where
    F: Fn(&'a [u8]) -> IResult<&'a [u8], T>,
{
    match params.get(key.as_bytes()) {
        None => bail_msg!("missing param {}", key),
        Some(v) => match parser(v.as_ref()) {
            IResult::Done(rest, v) => match match_eof(rest) {
                IResult::Done(..) => Ok(v),
                _ => bail_msg!("Unconsumed characters remain after parsing param"),
            },
            IResult::Incomplete(err) => bail_msg!("param parse incomplete: {:?}", err),
            IResult::Error(err) => bail_msg!("param parse failed: {:?}", err),
        },
    }
}

/// Given a hash of parameters, look up a parameter by name, and if it exists,
/// apply a parser to its value. If it doesn't, return the default value.
fn parseval_default<'a, F, T>(
    params: &'a HashMap<Vec<u8>, Vec<u8>>,
    key: &str,
    parser: F,
) -> Result<T>
where
    F: Fn(&'a [u8]) -> IResult<&'a [u8], T>,
    T: Default,
{
    match params.get(key.as_bytes()) {
        None => Ok(T::default()),
        Some(v) => match parser(v.as_ref()) {
            IResult::Done(unparsed, v) => match match_eof(unparsed) {
                IResult::Done(..) => Ok(v),
                _ => bail_msg!(
                    "Unconsumed characters remain after parsing param: {:?}",
                    unparsed
                ),
            },
            IResult::Incomplete(err) => bail_msg!("param parse incomplete: {:?}", err),
            IResult::Error(err) => bail_msg!("param parse failed: {:?}", err),
        },
    }
}

/// Given a hash of parameters, look up a parameter by name, and if it exists,
/// apply a parser to its value. If it doesn't, return None.
fn parseval_option<'a, F, T>(
    params: &'a HashMap<Vec<u8>, Vec<u8>>,
    key: &str,
    parser: F,
) -> Result<Option<T>>
where
    F: Fn(&'a [u8]) -> IResult<&'a [u8], T>,
{
    match params.get(key.as_bytes()) {
        None => Ok(None),
        Some(v) => match parser(v.as_ref()) {
            IResult::Done(unparsed, v) => match match_eof(unparsed) {
                IResult::Done(..) => Ok(Some(v)),
                _ => bail_msg!(
                    "Unconsumed characters remain after parsing param: {:?}",
                    unparsed
                ),
            },
            IResult::Incomplete(err) => bail_msg!("param parse incomplete: {:?}", err),
            IResult::Error(err) => bail_msg!("param parse failed: {:?}", err),
        },
    }
}

/// Parse a command, given some input, a command name (used as a tag), a param parser
/// function (which generalizes over batched and non-batched parameter syntaxes),
/// number of args (since each command has a fixed number of expected parameters,
/// not withstanding '*'), and a function to actually produce a parsed `SingleRequest`.
fn parse_command<'a, C, F, T>(
    inp: &'a [u8],
    cmd: C,
    parse_params: fn(&[u8], usize) -> IResult<&[u8], HashMap<Vec<u8>, Vec<u8>>>,
    nargs: usize,
    func: F,
) -> IResult<&'a [u8], T>
where
    F: Fn(HashMap<Vec<u8>, Vec<u8>>) -> Result<T>,
    C: AsRef<[u8]>,
{
    let cmd = cmd.as_ref();
    let res = do_parse!(
        inp,
        tag!(cmd) >> tag!("\n") >> p: call!(parse_params, nargs) >> (p)
    );

    match res {
        IResult::Done(rest, v) => {
            match func(v) {
                Ok(t) => IResult::Done(rest, t),
                Err(_e) => IResult::Error(Err::Code(ErrorKind::Custom(999999))), // ugh
            }
        }
        IResult::Error(e) => IResult::Error(e),
        IResult::Incomplete(n) => IResult::Incomplete(n),
    }
}

named!(ident_string<&[u8], String>,
    map!(ident_complete(ident), |x| String::from_utf8_lossy(x).into_owned())
);

named!(ident_string_alphanum<&[u8], String>,
    map!(ident_complete(ident_alphanum), |x| String::from_utf8_lossy(x).into_owned())
);

named!(percent_decoded<&[u8], Vec<u8>>,
    map!(
        ident_complete(ident),
        |xs| percent_decode(xs).if_any().unwrap_or(xs.to_vec())
    )
);

named!(percent_decoded_string<&[u8], String>,
    map_res!(percent_decoded, String::from_utf8)
);

/// Parse utf8 string, assumes that input is complete
fn utf8_string_complete(inp: &[u8]) -> IResult<&[u8], String> {
    match String::from_utf8(Vec::from(inp)) {
        Ok(s) => IResult::Done(b"", s),
        Err(_) => IResult::Error(Err::Code(ErrorKind::Custom(BAD_UTF8_ERR_CODE))),
    }
}

fn bytes_complete(inp: &[u8]) -> IResult<&[u8], Bytes> {
    let res = Bytes::from(inp);
    IResult::Done(b"", res)
}

// Parses list of bundlecaps values separated with encoded comma: 1,x,y
named!(bundlecaps_values_list<&[u8], HashSet<String>>,
    alt_complete!(
        map!(separated_list_complete!(tag!(","), percent_decoded_string), HashSet::from_iter) |
        map!(percent_decoded_string, |id| HashSet::from_iter(vec!(id)))
    )
);

// Parses bundlecap param with encoded commas and colon or equal chars: list=1,x,ys
named!(bundlecaps_param<&[u8], (String, HashSet<String>)>,
    alt_complete!(
        separated_pair!(percent_decoded_string, tag!("="), bundlecaps_values_list) |
        map!(percent_decoded_string, |id| (id, HashSet::new()))
    )
);

// Parses bundlecap params with encoded commas, equal and empty space chars: "block list=1,x,y"
named!(bundlecaps_params<&[u8], HashMap<String, HashSet<String>>>,
    map!(
        separated_list_complete!(tag!("\n"), bundlecaps_param),
        |x| HashMap::from_iter(x)
    )
);

// Decodes bundlecaps string from percent encoding and then run bundlecaps parser on top of it.
// Here you can see mostly flat_map! macro logic of nom, but because of inline
// Rust can resolve slice borrowing
fn bundlecaps_params_decoded(input: &[u8]) -> IResult<&[u8], HashMap<String, HashSet<String>>> {
    match percent_decoded(input) {
        IResult::Done(rest, y) => {
            match bundlecaps_params(
                y.as_slice(), /* This borrowing would not be possible with nom flat_map! combinator */
            ) {
                IResult::Done(_, z) => IResult::Done(rest, z),
                IResult::Incomplete(n) => IResult::Incomplete(n),
                IResult::Error(e) => IResult::Error(Err::Code(e.into_error_kind())),
            }
        }
        IResult::Incomplete(n) => IResult::Incomplete(n),
        IResult::Error(e) => IResult::Error(e),
    }
}

// Parses bundlecap argument
named!(
    bundlecaps_arg<&[u8], (String, HashMap<String, HashSet<String>>)>,
    alt_complete!(
        separated_pair!(ident_string, char!('='), bundlecaps_params_decoded) |
        map!(ident_string, |x| (x, HashMap::new()))
    )
);

// Parses bundlecap arguments list
named!(
    bundlecaps_args<&[u8], HashMap<String, HashMap<String, HashSet<String>>>>,
    map!(separated_list_complete!(char!(','), bundlecaps_arg), HashMap::from_iter)
);

macro_rules! replace_expr {
    ($_t:tt $sub:expr) => {
        $sub
    };
}

macro_rules! count_tts {
    ($($tts:tt)*) => {0usize $(+ replace_expr!($tts 1usize))*};
}

/// Macro to take a spec of a mercurial wire protocol command and generate the
/// code to invoke a parser for it. This works for "regular" commands with a
/// fixed number of named parameters.
macro_rules! command_common {
    // No parameters
    ( $i:expr, $name:expr, $req:ident, $star:expr, $parseparam:expr, { } ) => {
        call!($i, parse_command, $name, $parseparam, $star, |_| Ok($req))
    };

    // One key/parser pair for each parameter
    ( $i:expr, $name:expr, $req:ident, $star:expr, $parseparam:expr,
            { $( ($key:ident, $parser:expr) )+ } ) => {
        call!($i, parse_command, $name, $parseparam, $star+count_tts!( $($key)+ ),
            |kv| Ok($req {
                $( $key: parseval(&kv, stringify!($key), $parser)?, )*
            })
        )
    };
}

macro_rules! command {
    ( $i:expr, $name:expr, $req:ident, $parseparam:expr,
            { $( $key:ident => $parser:expr, )* } ) => {
        command_common!($i, $name, $req, 0, $parseparam, { $(($key, $parser))* } )
    };
}

macro_rules! command_star {
    ( $i:expr, $name:expr, $req:ident, $parseparam:expr,
            { $( $key:ident => $parser:expr, )* } ) => {
        command_common!($i, $name, $req, 1, $parseparam, { $(($key, $parser))* } )
    };
}

/// Parse a non-batched command
fn parse_singlerequest(inp: &[u8]) -> IResult<&[u8], SingleRequest> {
    parse_with_params(inp, params)
}

struct Batch {
    cmds: Vec<(Vec<u8>, Vec<u8>)>,
}

fn parse_batchrequest(inp: &[u8]) -> IResult<&[u8], Vec<SingleRequest>> {
    fn parse_cmd(inp: &[u8]) -> IResult<&[u8], SingleRequest> {
        parse_with_params(inp, batch_params)
    }

    let (rest, batch) = try_parse!(
        inp,
        command_star!("batch", Batch, params, {
            cmds => cmdlist,
        })
    );

    let mut parsed_cmds = Vec::with_capacity(batch.cmds.len());
    for cmd in batch.cmds {
        let full_cmd = Bytes::from([cmd.0, cmd.1].join(&b'\n'));
        // Jump through hoops to prevent the lifetime of `full_cmd` from leaking into the IResult
        // via errors.
        let cmd = match complete!(&full_cmd[..], parse_cmd) {
            IResult::Done(rest, out) => {
                if !rest.is_empty() {
                    return IResult::Error(Err::Code(ErrorKind::Eof));
                };
                out
            }
            IResult::Incomplete(need) => return IResult::Incomplete(need),
            IResult::Error(err) => return IResult::Error(Err::Code(err.into_error_kind())),
        };
        parsed_cmds.push(cmd);
    }
    IResult::Done(rest, parsed_cmds)
}

pub fn parse_request(buf: &mut BytesMut) -> Result<Option<Request>> {
    let res = {
        let origlen = buf.len();
        let parse_res = alt!(
            &buf[..],
            map!(parse_batchrequest, Request::Batch) | map!(parse_singlerequest, Request::Single)
        );

        match parse_res {
            IResult::Done(rest, val) => Some((origlen - rest.len(), val)),
            IResult::Incomplete(_) => None,
            IResult::Error(err) => {
                println!("{:?}", err);
                Err(errors::ErrorKind::CommandParse(
                    String::from_utf8_lossy(buf.as_ref()).into_owned(),
                ))?
            }
        }
    };

    Ok(res.map(|(consume, val)| {
        let _ = buf.split_to(consume);
        val
    }))
}

/// Common parser, generalized over how to parse parameters (either unbatched or
/// batched syntax.)
#[cfg_attr(rustfmt, rustfmt_skip)]
fn parse_with_params(
    inp: &[u8],
    parse_params: fn(&[u8], usize)
        -> IResult<&[u8], HashMap<Vec<u8>, Vec<u8>>>,
) -> IResult<&[u8], SingleRequest> {
    use SingleRequest::*;

    alt!(inp,
          command!("between", Between, parse_params, {
              pairs => pairlist,
          })
        | command!("branchmap", Branchmap, parse_params, {})
        | command!("capabilities", Capabilities, parse_params, {})
        | call!(parse_command, "debugwireargs", parse_params, 2+1,
            |kv| Ok(Debugwireargs {
                one: parseval(&kv, "one", ident_complete(ident_alphanum))?.to_vec(),
                two: parseval(&kv, "two", ident_complete(ident_alphanum))?.to_vec(),
                all_args: kv,
            }))
        | call!(parse_command, "getbundle", parse_params, 0+1,
            |kv| Ok(Getbundle(GetbundleArgs {
                // Some params are currently ignored, like:
                // - obsmarkers
                // - cg
                // - cbattempted
                // If those params are needed, they should be parsed here.
                heads: parseval_default(&kv, "heads", hashlist)?,
                common: parseval_default(&kv, "common", hashlist)?,
                bundlecaps: parseval_default(&kv, "bundlecaps", bundlecaps_args)?,
                listkeys: parseval_default(&kv, "listkeys", commavalues)?,
                phases: parseval_default(&kv, "phases", boolean)?,
            })))
        | command!("heads", Heads, parse_params, {})
        | command!("hello", Hello, parse_params, {})
        | command!("listkeys", Listkeys, parse_params, {
              namespace => ident_string_alphanum,
          })
        | command!("lookup", Lookup, parse_params, {
              key => utf8_string_complete,
          })
        | command_star!("known", Known, parse_params, {
              nodes => hashlist,
          })
        | command!("unbundle", Unbundle, parse_params, {
              heads => stringlist,
          })
        | call!(parse_command, "gettreepack", parse_params, 0+1,
            |kv| Ok(Gettreepack(GettreepackArgs {
                rootdir: parseval(&kv, "rootdir", bytes_complete)?,
                mfnodes: parseval(&kv, "mfnodes", hashlist)?,
                basemfnodes: parseval(&kv, "basemfnodes", hashlist)?,
                directories: parseval(&kv, "directories", gettreepack_directories)?,
                depth: parseval_option(&kv, "depth", closure!(
                    map_res!(
                        map_res!(take_while1!(is_digit), str::from_utf8),
                        usize::from_str
                    )
                ))?,
            })))
        | command!("getfiles", Getfiles, parse_params, {})
        | call!(parse_command, "stream_out_shallow", parse_params, 0+1, |_kv| Ok(StreamOutShallow))
    )
}

/// Test individual combinators
#[cfg(test)]
mod test {
    use super::*;
    use mercurial_types_mocks::nodehash::NULL_HASH;

    #[test]
    fn test_integer() {
        assert_eq!(integer(b"1234 "), IResult::Done(&b" "[..], 1234));
        assert_eq!(integer(b"1234"), IResult::Incomplete(Needed::Unknown));
    }

    #[test]
    fn test_ident() {
        assert_eq!(ident(b"01 "), IResult::Done(&b" "[..], &b"01"[..]));
        assert_eq!(ident(b"foo"), IResult::Done(&b""[..], &b"foo"[..]));
        assert_eq!(ident(b"foo "), IResult::Done(&b" "[..], &b"foo"[..]));
    }

    #[test]
    fn test_ident_alphanum() {
        assert_eq!(
            ident_alphanum(b"1234 "),
            IResult::Error(Err::Code(ErrorKind::AlphaNumeric))
        );
        assert_eq!(
            ident_alphanum(b" 1234 "),
            IResult::Error(Err::Code(ErrorKind::AlphaNumeric))
        );
        assert_eq!(ident_alphanum(b"foo"), IResult::Incomplete(Needed::Unknown));
        assert_eq!(
            ident_alphanum(b"foo "),
            IResult::Done(&b" "[..], &b"foo"[..])
        );
    }

    #[test]
    fn test_param_star() {
        let p = b"* 0\ntrailer";
        assert_eq!(param_star(p), IResult::Done(&b"trailer"[..], hashmap! {}));

        let p = b"* 1\n\
                  foo 12\n\
                  hello world!trailer";
        assert_eq!(
            param_star(p),
            IResult::Done(
                &b"trailer"[..],
                hashmap! {
                    b"foo".to_vec() => b"hello world!".to_vec(),
                }
            )
        );

        let p = b"* 2\n\
                  foo 12\n\
                  hello world!\
                  bar 4\n\
                  bloptrailer";
        assert_eq!(
            param_star(p),
            IResult::Done(
                &b"trailer"[..],
                hashmap! {
                    b"foo".to_vec() => b"hello world!".to_vec(),
                    b"bar".to_vec() => b"blop".to_vec(),
                }
            )
        );

        // no trailer
        let p = b"* 0\n";
        assert_eq!(param_star(p), IResult::Done(&b""[..], hashmap! {}));

        let p = b"* 1\n\
                  foo 12\n\
                  hello world!";
        assert_eq!(
            param_star(p),
            IResult::Done(
                &b""[..],
                hashmap! {
                    b"foo".to_vec() => b"hello world!".to_vec(),
                }
            )
        );
    }

    #[test]
    fn test_param_kv() {
        let p = b"foo 12\n\
                  hello world!trailer";
        assert_eq!(
            param_kv(p),
            IResult::Done(
                &b"trailer"[..],
                hashmap! {
                    b"foo".to_vec() => b"hello world!".to_vec(),
                }
            )
        );

        let p = b"foo 12\n\
                  hello world!";
        assert_eq!(
            param_kv(p),
            IResult::Done(
                &b""[..],
                hashmap! {
                    b"foo".to_vec() => b"hello world!".to_vec(),
                }
            )
        );
    }

    #[test]
    fn test_params() {
        let p = b"bar 12\n\
                  hello world!\
                  foo 7\n\
                  blibble\
                  very_long_key_no_data 0\n\
                  is_ok 1\n\
                  y\n\
                  badly formatted thing ";

        match params(p, 1) {
            IResult::Done(_, v) => assert_eq!(
                v,
                hashmap! {
                    b"bar".to_vec() => b"hello world!".to_vec(),
                }
            ),
            bad => panic!("bad result {:?}", bad),
        }

        match params(p, 2) {
            IResult::Done(_, v) => assert_eq!(
                v,
                hashmap! {
                    b"bar".to_vec() => b"hello world!".to_vec(),
                    b"foo".to_vec() => b"blibble".to_vec(),
                }
            ),
            bad => panic!("bad result {:?}", bad),
        }

        match params(p, 4) {
            IResult::Done(b"\nbadly formatted thing ", v) => assert_eq!(
                v,
                hashmap! {
                    b"bar".to_vec() => b"hello world!".to_vec(),
                    b"foo".to_vec() => b"blibble".to_vec(),
                    b"very_long_key_no_data".to_vec() => b"".to_vec(),
                    b"is_ok".to_vec() => b"y".to_vec(),
                }
            ),
            bad => panic!("bad result {:?}", bad),
        }

        match params(p, 5) {
            IResult::Error(Err::Position(ErrorKind::Alt, _)) => (),
            bad => panic!("bad result {:?}", bad),
        }

        match params(&p[..3], 1) {
            IResult::Incomplete(_) => (),
            bad => panic!("bad result {:?}", bad),
        }

        for l in 0..p.len() {
            match params(&p[..l], 4) {
                IResult::Incomplete(_) => (),
                IResult::Done(remain, ref kv) => {
                    assert_eq!(kv.len(), 4);
                    assert!(
                        b"\nbadly formatted thing ".starts_with(remain),
                        "remain \"{:?}\"",
                        remain
                    );
                }
                bad => panic!("bad result l {} bad {:?}", l, bad),
            }
        }
    }

    #[test]
    fn test_params_star() {
        let star = b"* 1\n\
                     foo 0\n\
                     bar 0\n";
        match params(star, 2) {
            IResult::Incomplete(_) => panic!("unexpectedly incomplete"),
            IResult::Done(remain, kv) => {
                assert_eq!(remain, b"");
                assert_eq!(
                    kv,
                    hashmap! {
                        b"foo".to_vec() => vec!{},
                        b"bar".to_vec() => vec!{},
                    }
                );
            }
            IResult::Error(err) => panic!("unexpected error {:?}", err),
        }

        let star = b"* 2\n\
                     foo 0\n\
                     plugh 0\n\
                     bar 0\n";
        match params(star, 2) {
            IResult::Incomplete(_) => panic!("unexpectedly incomplete"),
            IResult::Done(remain, kv) => {
                assert_eq!(remain, b"");
                assert_eq!(
                    kv,
                    hashmap! {
                        b"foo".to_vec() => vec!{},
                        b"bar".to_vec() => vec!{},
                        b"plugh".to_vec() => vec!{},
                    }
                );
            }
            IResult::Error(err) => panic!("unexpected error {:?}", err),
        }

        let star = b"* 0\n\
                     bar 0\n";
        match params(star, 2) {
            IResult::Incomplete(_) => panic!("unexpectedly incomplete"),
            IResult::Done(remain, kv) => {
                assert_eq!(remain, b"");
                assert_eq!(
                    kv,
                    hashmap! {
                        b"bar".to_vec() => vec!{},
                    }
                );
            }
            IResult::Error(err) => panic!("unexpected error {:?}", err),
        }

        match params(&star[..4], 2) {
            IResult::Incomplete(_) => (),
            IResult::Done(remain, kv) => panic!("unexpected Done remain {:?} kv {:?}", remain, kv),
            IResult::Error(err) => panic!("unexpected error {:?}", err),
        }
    }

    #[test]
    fn test_batch_param_escaped() {
        let p = b"foo=b:ear";

        assert_eq!(
            batch_param_escaped(p),
            IResult::Done(&b""[..], (b"foo".to_vec(), b"b=ar".to_vec()))
        );
    }

    #[test]
    fn test_batch_params() {
        let p = b"foo=bar";

        assert_eq!(
            batch_params(p, 0),
            IResult::Done(
                &b""[..],
                hashmap! {
                    b"foo".to_vec() => b"bar".to_vec(),
                }
            )
        );

        let p = b"foo=bar,biff=bop,esc:c:o:s:e=esc:c:o:s:e";

        assert_eq!(
            batch_params(p, 0),
            IResult::Done(
                &b""[..],
                hashmap! {
                    b"foo".to_vec() => b"bar".to_vec(),
                    b"biff".to_vec() => b"bop".to_vec(),
                    b"esc:,;=".to_vec() => b"esc:,;=".to_vec(),
                }
            )
        );

        let p = b"";

        assert_eq!(batch_params(p, 0), IResult::Done(&b""[..], hashmap! {}));

        let p = b"foo=";

        assert_eq!(
            batch_params(p, 0),
            IResult::Done(&b""[..], hashmap! {b"foo".to_vec() => b"".to_vec()})
        );
    }

    #[test]
    fn test_nodehash() {
        assert_eq!(
            nodehash(b"0000000000000000000000000000000000000000"),
            IResult::Done(&b""[..], NULL_HASH)
        );

        assert_eq!(
            nodehash(b"000000000000000000000000000000x000000000")
                .map_err(|err| Err::Code(err.into_error_kind())),
            IResult::Error(Err::Code(ErrorKind::MapRes,))
        );

        assert_eq!(
            nodehash(b"000000000000000000000000000000000000000"),
            IResult::Incomplete(Needed::Size(40))
        );
    }

    #[test]
    fn test_parseval_extra_characters() {
        let kv = hashmap! {
        b"foo".to_vec() => b"0000000000000000000000000000000000000000extra".to_vec(),
        };
        match parseval(&kv, "foo", hashlist) {
            Err(_) => (),
            _ => panic!(
                "Paramval parse failed: Did not raise an error for param\
                 with trailing characters."
            ),
        }
    }

    #[test]
    fn test_parseval_default_extra_characters() {
        let kv = hashmap! {
        b"foo".to_vec() => b"0000000000000000000000000000000000000000extra".to_vec(),
        };
        match parseval_default(&kv, "foo", hashlist) {
            Err(_) => (),
            _ => panic!(
                "paramval_default parse failed: Did not raise an error for param\
                 with trailing characters."
            ),
        }
    }

    #[test]
    fn test_pair() {
        let p =
            b"0000000000000000000000000000000000000000-0000000000000000000000000000000000000000";
        assert_eq!(pair(p), IResult::Done(&b""[..], (NULL_HASH, NULL_HASH)));

        assert_eq!(pair(&p[..80]), IResult::Incomplete(Needed::Size(81)));

        assert_eq!(pair(&p[..41]), IResult::Incomplete(Needed::Size(81)));

        assert_eq!(pair(&p[..40]), IResult::Incomplete(Needed::Size(41)));
    }

    #[test]
    fn test_pairlist() {
        let p =
            b"0000000000000000000000000000000000000000-0000000000000000000000000000000000000000 \
              0000000000000000000000000000000000000000-0000000000000000000000000000000000000000";
        assert_eq!(
            pairlist(p),
            IResult::Done(
                &b""[..],
                vec![(NULL_HASH, NULL_HASH), (NULL_HASH, NULL_HASH)]
            )
        );

        let p =
            b"0000000000000000000000000000000000000000-0000000000000000000000000000000000000000";
        assert_eq!(
            pairlist(p),
            IResult::Done(&b""[..], vec![(NULL_HASH, NULL_HASH)])
        );

        let p = b"";
        assert_eq!(pairlist(p), IResult::Done(&b""[..], vec![]));

        let p = b"0000000000000000000000000000000000000000-00000000000000";
        assert_eq!(
            pairlist(p),
            IResult::Done(
                &b"0000000000000000000000000000000000000000-00000000000000"[..],
                vec![]
            )
        );
    }

    #[test]
    fn test_hashlist() {
        let p =
            b"0000000000000000000000000000000000000000 0000000000000000000000000000000000000000 \
              0000000000000000000000000000000000000000 0000000000000000000000000000000000000000";
        assert_eq!(
            hashlist(p),
            IResult::Done(&b""[..], vec![NULL_HASH, NULL_HASH, NULL_HASH, NULL_HASH])
        );

        let p = b"0000000000000000000000000000000000000000";
        assert_eq!(hashlist(p), IResult::Done(&b""[..], vec![NULL_HASH]));

        let p = b"";
        assert_eq!(hashlist(p), IResult::Done(&b""[..], vec![]));

        // incomplete should leave bytes on the wire
        let p = b"00000000000000000000000000000";
        assert_eq!(
            hashlist(p),
            IResult::Done(&b"00000000000000000000000000000"[..], vec![])
        );
    }

    #[test]
    fn test_commavalues() {
        // Empty list
        let p = b"";
        assert_eq!(commavalues(p), IResult::Done(&b""[..], vec![]));

        // Single entry
        let p = b"abc";
        assert_eq!(
            commavalues(p),
            IResult::Done(&b""[..], vec![b"abc".to_vec()])
        );

        // Multiple entries
        let p = b"123,abc,test,456";
        assert_eq!(
            commavalues(p),
            IResult::Done(
                &b""[..],
                vec![
                    b"123".to_vec(),
                    b"abc".to_vec(),
                    b"test".to_vec(),
                    b"456".to_vec(),
                ]
            )
        );
    }

    #[test]
    fn test_cmd() {
        let p = b"foo bar";

        assert_eq!(
            cmd(p),
            IResult::Done(&b""[..], (b"foo".to_vec(), b"bar".to_vec()))
        );

        let p = b"noparam ";
        assert_eq!(
            cmd(p),
            IResult::Done(&b""[..], (b"noparam".to_vec(), b"".to_vec()))
        );
    }

    #[test]
    fn test_cmdlist() {
        let p = b"foo bar";

        assert_eq!(
            cmdlist(p),
            IResult::Done(&b""[..], vec![(b"foo".to_vec(), b"bar".to_vec())])
        );

        let p = b"foo bar;biff blop";

        assert_eq!(
            cmdlist(p),
            IResult::Done(
                &b""[..],
                vec![
                    (b"foo".to_vec(), b"bar".to_vec()),
                    (b"biff".to_vec(), b"blop".to_vec()),
                ]
            )
        );
    }
}

/// Test parsing each command
#[cfg(test)]
mod test_parse {
    use super::*;
    use std::fmt::Debug;

    fn hash_ones() -> HgNodeHash {
        "1111111111111111111111111111111111111111".parse().unwrap()
    }

    fn hash_twos() -> HgNodeHash {
        "2222222222222222222222222222222222222222".parse().unwrap()
    }

    fn hash_threes() -> HgNodeHash {
        "3333333333333333333333333333333333333333".parse().unwrap()
    }

    fn hash_fours() -> HgNodeHash {
        "4444444444444444444444444444444444444444".parse().unwrap()
    }

    /// Common code for testing parsing:
    /// - check all truncated inputs return "Ok(None)"
    /// - complete inputs return the expected result, and leave any remainder in
    ///    the input buffer.
    fn test_parse<I: AsRef<[u8]> + Debug>(inp: I, exp: Request) {
        test_parse_with_extra(inp, exp, b"extra")
    }

    fn test_parse_with_extra<I>(inp: I, exp: Request, extra: &[u8])
    where
        I: AsRef<[u8]> + Debug,
    {
        let inbytes = inp.as_ref();

        // check for short inputs
        for l in 0..inbytes.len() - 1 {
            let mut buf = BytesMut::from(inbytes[0..l].to_vec());
            match parse_request(&mut buf) {
                Ok(None) => (),
                Ok(Some(val)) => panic!(
                    "BAD PASS: inp >>{:?}<< lpassed unexpectedly val {:?} pass with {}/{} bytes",
                    Bytes::from(inbytes.to_vec()),
                    val,
                    l,
                    inbytes.len()
                ),
                Err(err) => panic!(
                    "BAD FAIL: inp >>{:?}<< failed {:?} (not incomplete) with {}/{} bytes",
                    Bytes::from(inbytes.to_vec()),
                    err,
                    l,
                    inbytes.len()
                ),
            };
        }

        // check for exact and extra
        for l in 0..extra.len() {
            let mut buf = BytesMut::from(inbytes.to_vec());
            buf.extend_from_slice(&extra[0..l]);
            let buflen = buf.len();
            match parse_request(&mut buf) {
                Ok(Some(val)) => assert_eq!(val, exp, "with {}/{} bytes", buflen, inbytes.len()),
                Ok(None) => panic!(
                    "BAD INCOMPLETE: inp >>{:?}<< extra {} incomplete {}/{} bytes",
                    Bytes::from(inbytes.to_vec()),
                    l,
                    buflen,
                    inbytes.len()
                ),
                Err(err) => panic!(
                    "BAD FAIL: inp >>{:?}<< extra {} failed {:?} (not incomplete) with {}/{} bytes",
                    Bytes::from(inbytes.to_vec()),
                    l,
                    err,
                    buflen,
                    inbytes.len()
                ),
            };
            assert_eq!(&*buf, &extra[0..l]);
        }
    }

    #[test]
    fn test_parse_batch_1() {
        let inp = "batch\n\
                   * 0\n\
                   cmds 6\n\
                   hello ";

        test_parse(inp, Request::Batch(vec![SingleRequest::Hello]))
    }

    #[test]
    fn test_parse_batch_2() {
        let inp = "batch\n\
                   * 0\n\
                   cmds 12\n\
                   known nodes=";

        test_parse(
            inp,
            Request::Batch(vec![SingleRequest::Known { nodes: vec![] }]),
        )
    }

    #[test]
    fn test_parse_batch_3() {
        let inp = "batch\n\
                   * 0\n\
                   cmds 19\n\
                   hello ;known nodes=";

        test_parse(
            inp,
            Request::Batch(vec![
                SingleRequest::Hello,
                SingleRequest::Known { nodes: vec![] },
            ]),
        )
    }

    #[test]
    fn test_parse_between() {
        let inp =
            "between\n\
             pairs 163\n\
             1111111111111111111111111111111111111111-2222222222222222222222222222222222222222 \
             3333333333333333333333333333333333333333-4444444444444444444444444444444444444444";
        test_parse(
            inp,
            Request::Single(SingleRequest::Between {
                pairs: vec![(hash_ones(), hash_twos()), (hash_threes(), hash_fours())],
            }),
        );
    }

    #[test]
    fn test_parse_branchmap() {
        let inp = "branchmap\n";

        test_parse(inp, Request::Single(SingleRequest::Branchmap {}));
    }

    #[test]
    fn test_parse_capabilities() {
        let inp = "capabilities\n";

        test_parse(inp, Request::Single(SingleRequest::Capabilities {}));
    }

    #[test]
    fn test_parse_debugwireargs() {
        let inp = "debugwireargs\n\
                   * 2\n\
                   three 5\nTHREE\
                   empty 0\n\
                   one 3\nONE\
                   two 3\nTWO";
        test_parse(
            inp,
            Request::Single(SingleRequest::Debugwireargs {
                one: b"ONE".to_vec(),
                two: b"TWO".to_vec(),
                all_args: hashmap! {
                    b"one".to_vec() => b"ONE".to_vec(),
                    b"two".to_vec() => b"TWO".to_vec(),
                    b"three".to_vec() => b"THREE".to_vec(),
                    b"empty".to_vec() => vec![],
                },
            }),
        );
    }

    #[test]
    fn test_parse_getbundle() {
        // with no arguments
        let inp = "getbundle\n\
                   * 0\n";

        test_parse(
            inp,
            Request::Single(SingleRequest::Getbundle(GetbundleArgs {
                heads: vec![],
                common: vec![],
                bundlecaps: HashMap::new(),
                listkeys: vec![],
                phases: false,
            })),
        );

        // with arguments
        let inp =
            "getbundle\n\
             * 6\n\
             heads 40\n\
             1111111111111111111111111111111111111111\
             common 81\n\
             2222222222222222222222222222222222222222 3333333333333333333333333333333333333333\
             bundlecaps 14\n\
             cap1,CAP2,cap3\
             listkeys 9\n\
             key1,key2\
             phases 1\n\
             1\
             extra 5\n\
             extra";
        test_parse(
            inp,
            Request::Single(SingleRequest::Getbundle(GetbundleArgs {
                heads: vec![hash_ones()],
                common: vec![hash_twos(), hash_threes()],
                bundlecaps: ["cap1", "CAP2", "cap3"]
                    .iter()
                    .map(|s| (s.to_string(), HashMap::new()))
                    .collect(),
                listkeys: vec![b"key1".to_vec(), b"key2".to_vec()],
                phases: true,
            })),
        );
    }

    #[test]
    fn test_parse_getbundle_bundlecaps() {
        let inp = "getbundle\n\
                   * 1\n\
                   bundlecaps 78\n\
                   block,entries=entry%0Alist%3D1%2Cx%2Cy%0Asingle%3Dx%0Adoubleenc%3Dtest%253A123";

        let expected_entries_list: HashSet<String> =
            ["1", "x", "y"].iter().map(|x| x.to_string()).collect();

        let expected_single: HashSet<String> = ["x"].iter().map(|x| x.to_string()).collect();

        let expected_doubleenc: HashSet<String> =
            ["test:123"].iter().map(|x| x.to_string()).collect();

        let expected_entries: HashMap<String, HashSet<String>> = [
            ("entry".to_string(), HashSet::new()),
            ("list".to_string(), expected_entries_list),
            ("single".to_string(), expected_single),
            ("doubleenc".to_string(), expected_doubleenc),
        ]
        .iter()
        .cloned()
        .collect();

        let expected_bundlecaps: HashMap<String, HashMap<String, HashSet<String>>> = [
            ("block".to_string(), HashMap::new()),
            ("entries".to_string(), expected_entries),
        ]
        .iter()
        .cloned()
        .collect();

        test_parse(
            inp,
            Request::Single(SingleRequest::Getbundle(GetbundleArgs {
                heads: vec![],
                common: vec![],
                bundlecaps: expected_bundlecaps,
                listkeys: vec![],
                phases: false,
            })),
        );
    }

    #[test]
    fn test_parse_heads() {
        let inp = "heads\n";

        test_parse(inp, Request::Single(SingleRequest::Heads {}));
    }

    #[test]
    fn test_parse_hello() {
        let inp = "hello\n";

        test_parse(inp, Request::Single(SingleRequest::Hello {}));
    }

    #[test]
    fn test_parse_listkeys() {
        let inp = "listkeys\n\
                   namespace 9\n\
                   bookmarks";

        test_parse(
            inp,
            Request::Single(SingleRequest::Listkeys {
                namespace: "bookmarks".to_string(),
            }),
        );
    }

    #[test]
    fn test_parse_lookup() {
        let inp = "lookup\n\
                   key 9\n\
                   bookmarks";

        test_parse(
            inp,
            Request::Single(SingleRequest::Lookup {
                key: "bookmarks".to_string(),
            }),
        );
    }

    #[test]
    fn test_parse_lookup2() {
        let inp = "lookup\n\
                   key 4\n\
                   5c79";

        test_parse(
            inp,
            Request::Single(SingleRequest::Lookup {
                key: "5c79".to_string(),
            }),
        );
    }

    #[test]
    fn test_parse_gettreepack() {
        let inp = "gettreepack\n\
                   * 4\n\
                   rootdir 0\n\
                   mfnodes 40\n\
                   1111111111111111111111111111111111111111\
                   basemfnodes 40\n\
                   1111111111111111111111111111111111111111\
                   directories 0\n";

        test_parse(
            inp,
            Request::Single(SingleRequest::Gettreepack(GettreepackArgs {
                rootdir: Bytes::new(),
                mfnodes: vec![hash_ones()],
                basemfnodes: vec![hash_ones()],
                directories: vec![],
                depth: None,
            })),
        );

        let inp =
            "gettreepack\n\
             * 5\n\
             depth 1\n\
             1\
             rootdir 5\n\
             ololo\
             mfnodes 81\n\
             1111111111111111111111111111111111111111 2222222222222222222222222222222222222222\
             basemfnodes 81\n\
             2222222222222222222222222222222222222222 1111111111111111111111111111111111111111\
             directories 5\n\
             :o,:s";

        test_parse(
            inp,
            Request::Single(SingleRequest::Gettreepack(GettreepackArgs {
                rootdir: Bytes::from("ololo".as_bytes()),
                mfnodes: vec![hash_ones(), hash_twos()],
                basemfnodes: vec![hash_twos(), hash_ones()],
                directories: vec![Bytes::from(",".as_bytes()), Bytes::from(";".as_bytes())],
                depth: Some(1),
            })),
        );
    }

    #[test]
    fn test_parse_known_1() {
        let inp = "known\n\
                   * 0\n\
                   nodes 40\n\
                   1111111111111111111111111111111111111111";

        test_parse(
            inp,
            Request::Single(SingleRequest::Known {
                nodes: vec![hash_ones()],
            }),
        );
    }

    #[test]
    fn test_parse_known_2() {
        let inp = "known\n\
                   * 0\n\
                   nodes 0\n";

        test_parse(inp, Request::Single(SingleRequest::Known { nodes: vec![] }));
    }

    fn test_parse_unbundle_with(bundle: &[u8]) {
        let inp = b"unbundle\n\
                    heads 10\n\
                    666f726365"; // "force" hex encoded

        test_parse_with_extra(
            inp,
            Request::Single(SingleRequest::Unbundle {
                heads: vec![String::from("666f726365")], // "force" in hex-encoding
            }),
            bundle,
        );
    }

    #[test]
    fn test_parse_unbundle_minimal() {
        let bundle: &[u8] = &b"HG20\0\0\0\0\0\0\0\0"[..];
        test_parse_unbundle_with(bundle);
    }

    #[test]
    fn test_parse_unbundle_small() {
        let bundle: &[u8] = &include_bytes!("../../fixtures/min.bundle")[..];
        test_parse_unbundle_with(bundle);
    }

    #[test]
    fn test_batch_parse_heads() {
        match parse_with_params(b"heads\n", batch_params) {
            IResult::Done(rest, val) => {
                assert!(rest.is_empty());
                assert_eq!(val, SingleRequest::Heads {});
            }
            IResult::Incomplete(_) => panic!("unexpected incomplete input"),
            IResult::Error(err) => panic!("failed with {:?}", err),
        }
    }

    #[test]
    fn test_parse_batch_heads() {
        let inp = "batch\n\
                   * 0\n\
                   cmds 116\n\
                   heads ;\
                   lookup key=1234;\
                   known nodes=1111111111111111111111111111111111111111 \
                   2222222222222222222222222222222222222222";

        test_parse(
            inp,
            Request::Batch(vec![
                SingleRequest::Heads {},
                SingleRequest::Lookup {
                    key: "1234".to_string(),
                },
                SingleRequest::Known {
                    nodes: vec![hash_ones(), hash_twos()],
                },
            ]),
        );
    }

    #[test]
    fn test_parse_stream_out_shallow() {
        let inp = "stream_out_shallow\n\
                   * 1\n\
                   noflatmanifest 4\n\
                   True";

        test_parse(inp, Request::Single(SingleRequest::StreamOutShallow));
    }

}
