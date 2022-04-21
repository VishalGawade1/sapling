#chg-compatible

test rust clone

  $ configure modern
  $ setconfig clone.use-rust=True
  $ setconfig remotefilelog.reponame=test-repo
  $ export LOG=hgcommands::commands::clone


 Prepare Source:

  $ newremoterepo repo1
  $ setconfig paths.default=test:e1
  $ drawdag << 'EOS'
  > E
  > |
  > D
  > |
  > C
  > |
  > B
  > |
  > A
  > EOS

  $ hg push -r $E --to master --create -q

Test that nonsupported options fallback to python:

  $ cd $TESTTMP
  $ hg clone test:e1 $TESTTMP/update-clone
  fetching lazy changelog
  populating main commit graph
  tip commit: 9bc730a19041f9ec7cb33c626e811aa233efb18c
  fetching selected remote bookmarks
  updating to branch default
  5 files updated, 0 files merged, 0 files removed, 0 files unresolved

  $ cd $TESTTMP
  $ hg clone -U -r $D test:e1 $TESTTMP/rev-clone
  fetching lazy changelog
  populating main commit graph
  tip commit: 9bc730a19041f9ec7cb33c626e811aa233efb18c
  fetching selected remote bookmarks

  $ git init -q git-source
  $ hg clone --git "$TESTTMP/git-source" $TESTTMP/git-clone

  $ hg clone -U --enable-profile test_profile test:e1 $TESTTMP/sparse-clone --config extensions.sparse=
  fetching lazy changelog
  populating main commit graph
  tip commit: 9bc730a19041f9ec7cb33c626e811aa233efb18c
  fetching selected remote bookmarks

Test rust clone
  $ hg clone -U test:e1 $TESTTMP/rust-clone
  TRACE hgcommands::commands::clone: performing rust clone
  TRACE hgcommands::commands::clone: fetching lazy commit data and bookmarks
  $ cd $TESTTMP/rust-clone

Check metalog is written and keys are tracked correctly
  $ hg dbsh -c 'ui.write(str(ml.get("remotenames")))'
  b'9bc730a19041f9ec7cb33c626e811aa233efb18c bookmarks remote/master\n' (no-eol)

Check configuration
  $ hg paths
  default = test:e1
  $ hg config remotefilelog.reponame
  test-repo

Check commits
  $ hg log -r tip -T "{desc}\n"
  E
  $ hg log -T "{desc}\n"
  E
  D
  C
  B
  A

Check basic operations
  $ hg up master
  5 files updated, 0 files merged, 0 files removed, 0 files unresolved
  $ echo newfile > newfile
  $ hg commit -Aqm 'new commit'

Test cloning with default destination
  $ cd $TESTTMP
  $ hg clone -U test:e1
  TRACE hgcommands::commands::clone: performing rust clone
  TRACE hgcommands::commands::clone: fetching lazy commit data and bookmarks
  $ cd test-repo
  $ hg log -r tip -T "{desc}\n"
  E
