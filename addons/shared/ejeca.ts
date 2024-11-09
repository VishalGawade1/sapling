/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import type {ChildProcess, IOType} from 'node:child_process';
import type {Stream} from 'node:stream';

import getStream from 'get-stream';
import {Readable} from 'node:stream';
import os from 'os';

export interface EjecaOptions {
  /**
   * Current working directory of the child process.
   * @default process.cwd()
   */
  readonly cwd?: string;

  /**
   * Environment key-value pairs. Extends automatically if `process.extendEnv` is set to true.
   * @default process.env
   */
  readonly env?: NodeJS.ProcessEnv;

  /**
   * Set to `false` if you don't want to extend the environment variables when providing the `env` property.
   * @default true
   */
  readonly extendEnv?: boolean;

  /**
   * Feeds its contents as the standard input of the binary.
   */
  readonly input?: string | Buffer | ReadableStream;

  /**
   * Setting this to `false` resolves the promise with the error instead of rejecting it.
   * @default true
   */
  readonly reject?: boolean;

  /**
   * Same options as [`stdio`](https://nodejs.org/docs/latest-v18.x/api/child_process.html#optionsstdio).
   * @default 'pipe'
   */
  readonly stdin?: IOType | Stream | number | null | undefined;

  /**
   * Strip the final newline character from the (awaitable) output.
   * @default true
   */
  readonly stripFinalNewline?: boolean;
}

interface KillOptions {
  /**
   * Milliseconds to wait for the child process to terminate before sending `SIGKILL`.
   * Can be disabled with `false`.
   * @default 5000
   */
  forceKillAfterTimeout?: number | boolean;
}

type KillParam = number | NodeJS.Signals | undefined;
const DEFAULT_FORCE_KILL_TIMEOUT = 1000 * 5;

function spawnedKill(
  kill: ChildProcess['kill'],
  signal: KillParam = 'SIGTERM',
  options: KillOptions = {},
): boolean {
  const killResult = kill(signal);

  if (shouldForceKill(signal, options, killResult)) {
    const timeout = getForceKillAfterTimeout(options);
    setTimeout(() => {
      kill('SIGKILL');
    }, timeout);
  }

  return killResult;
}

function getForceKillAfterTimeout({forceKillAfterTimeout = true}: KillOptions): number {
  if (typeof forceKillAfterTimeout !== 'number') {
    return DEFAULT_FORCE_KILL_TIMEOUT;
  }

  if (!Number.isFinite(forceKillAfterTimeout) || forceKillAfterTimeout < 0) {
    throw new TypeError(
      `Expected the \`forceKillAfterTimeout\` option to be a non-negative integer, got \`${forceKillAfterTimeout}\` (${typeof forceKillAfterTimeout})`,
    );
  }

  return forceKillAfterTimeout;
}

function shouldForceKill(
  signal: KillParam,
  {forceKillAfterTimeout}: KillOptions,
  killResult: boolean,
): boolean {
  const isSigTerm = signal === os.constants.signals.SIGTERM || signal == 'SIGTERM';
  return isSigTerm && forceKillAfterTimeout !== false && killResult;
}

export interface EjecaReturn {
  /**
   * The exit code if the child exited on its own.
   */
  exitCode: number;

  /**
   * The signal by which the child process was terminated, `undefined` if the process was not killed.
   *
   * Essentially obtained through `signal` on the `exit` event from [`ChildProcess`](https://nodejs.org/docs/latest-v18.x/api/child_process.html#event-exit)
   */
  signal?: string;

  /**
   * The file and arguments that were run, escaped. Useful for logging.
   */
  escapedCommand: string;

  /**
   * The output of the process on stdout.
   */
  stdout: string;

  /**
   * The output of the process on stderr.
   */
  stderr: string;

  /**
   * Whether the process was killed.
   */
  killed: boolean;
}

export interface EjecaError extends Error, EjecaReturn {}

interface EjecaChildPromise {
  catch<ResultType = never>(
    onRejected?: (reason: EjecaError) => ResultType | PromiseLike<ResultType>,
  ): Promise<EjecaReturn | ResultType>;

  /**
   * Essentially the same as [`subprocess.kill`](https://nodejs.org/docs/latest-v18.x/api/child_process.html#subprocesskillsignal), but
   * with the caveat of having the processes SIGKILL'ed after a few seconds if the original signal
   * didn't successfully terminate the process. This behavior is configurable through the `options` option.
   */
  kill(signal?: KillParam, options?: KillOptions): boolean;
}

export type EjecaChildProcess = ChildProcess & EjecaChildPromise & Promise<EjecaReturn>;

// The return value is a mixin of `childProcess` and `Promise`
function getMergePromise(
  spawned: ChildProcess,
  promise: Promise<EjecaReturn>,
): ChildProcess & Promise<EjecaReturn> {
  const s2 = Object.create(spawned);
  // @ts-expect-error: we are doing some good old monkey patching here
  s2.then = (...args) => {
    return promise.then(...args);
  };
  // @ts-expect-error: we are doing some good old monkey patching here
  s2.catch = (...args) => {
    return promise.catch(...args);
  };
  // @ts-expect-error: we are doing some good old monkey patching here
  s2.finally = (...args) => {
    return promise.finally(...args);
  };

  return s2 as unknown as ChildProcess & Promise<EjecaReturn>;
}

// Use promises instead of `child_process` events
async function getSpawnedPromise(spawned: ChildProcess): Promise<EjecaReturn> {
  const {stdout, stderr} = spawned;
  const spawnedPromise = new Promise<{exitCode: number; signal?: string}>((resolve, reject) => {
    spawned.on('exit', (exitCode, signal) => {
      resolve({exitCode: exitCode ?? 0, signal: signal ?? undefined});
    });

    spawned.on('error', error => {
      reject(error);
    });

    if (spawned.stdin) {
      spawned.stdin.on('error', error => {
        reject(error);
      });
    }
  });

  return Promise.all([spawnedPromise, getStreamPromise(stdout), getStreamPromise(stderr)]).then(
    values => {
      const [rc, stdout, stderr] = values;
      return {
        ...rc,
        stdout,
        stderr,
        killed: false,
        escapedCommand: '',
      };
    },
  );
}

async function getStreamPromise(origStream: Stream | null): Promise<string> {
  const stream = origStream ?? new Readable({read() {}});
  return getStream(stream, {encoding: 'utf8'});
}

/**
 * Essentially a wrapper for [`child_process.spawn`](https://nodejs.org/docs/latest-v18.x/api/child_process.html#child_processspawncommand-args-options), which
 * additionally makes the result awaitable through `EjecaChildPromise`. `_file`, `_argumentos` and `_options`
 * are essentially the same as the args for `child_process.spawn`.
 *
 * It also has a couple of additional features:
 * - Adds a forced timeout kill for `child_process.kill` through `EjecaChildPromise.kill`
 * - Allows feeding to stdin through `_options.input`
 */
export function ejeca(
  _file: string,
  _argumentos: readonly string[],
  _options?: EjecaOptions,
): EjecaChildProcess {
  throw new Error('Not implemented');
}
