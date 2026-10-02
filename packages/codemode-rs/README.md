# Code mode

`agent-codemode` provides native script orchestration for Maestro and Dex.
It brings the composition and context-filtering behavior described in
[Pi's code-mode documentation](https://github.com/earendil-works/pi/blob/9fba660cf1caca0ade5bea72269352416e595a19/packages/coding-agent/docs/codemode.md)
to the existing Rust tool boundaries. It does not own tool permissions,
credentials, effect admission, approvals, or durable receipts.

The model calls `codemode` with a `code` string containing the body of an async
JavaScript function. Independent reads can run in one wave; their results can
feed subsequent calls without another model request. Only `text()` output and
the returned value enter the model's tool-result context.

```javascript
const matches = ALL_TOOLS.filter(tool => /search/.test(tool.name));
text(matches.map(tool => ({name: tool.name, schema: tool.schema})));
```

```javascript
const results = await Promise.allSettled([
  tools.read({path: "Cargo.toml"}),
  tools.read({path: "README.md"})
]);
text(results.map(result => result.status === "fulfilled"
  ? {status: result.status, characters: result.value.length}
  : {status: result.status, error: result.reason.message}));
```

Tool names and available arguments come from the admitted catalog, not from
script-supplied metadata. Names containing punctuation also have an identifier
alias, such as `tools.read_rows` for `read.rows`. Ambiguous aliases fail closed.
The catalog records the identifier on each entry. Dex exposes its ordinary
core or already-discovered tools; Maestro preserves its allowed-tool set and
profile restrictions. Conversational questions and confirmations use direct
tool calls. Recursive scripts are unavailable.

Each script gets a fresh QuickJS VM embedded in the native binary. There is no
Node/Bun subprocess, host filesystem, network, module loader, process API, or
timer API. The host bridge emits tool requests; the composing agent applies
its normal authorization and execution path to each request. Permitted reads
run concurrently, while effects retain their existing ordered execution and
effect receipts. A script cannot authorize its own nested calls.

The VM has a 32 MiB heap, a 256 KiB stack, a maximum of 64 nested calls,
64 KiB of output, and a hard 60-second deadline. Existing host deadlines and
cancellation may stop it sooner. Output-limit violations remain failures even
when the script catches the exception. A promise with no pending tool call
fails immediately. Unawaited requests are discarded when the script finishes.

Failed tool calls reject their JavaScript promises. A script failure preserves
partial output; completed effects are not undone. Each composing agent retains
the underlying call evidence separately from the script's model-facing summary.
Restart handling preserves the existing refusal to replay an uncertain effect.

The VM has no durable store. Script-local variables cover dependencies within
one call; existing agent journals remain the owners of execution state. This
change does not import Pi's provider transports, model-running helpers, or
session-storage format.

The native engine is pinned to `rquickjs` 0.13.0, published September 8, 2026,
to retain the repository's fourteen-day dependency cooling period. Independent
archive review verified the three package checksums in both Cargo locks.
The exact `rquickjs-sys` build-script admission expires October 17, 2026.
With default features disabled, native builds compile the vendored QuickJS C
sources through `cc` and use bundled bindings. The optional WASI SDK downloader,
bindgen, and dynamic loader are outside this native integration.
The reviewed sys archive SHA-256 is
`53d0aaff245bed1c6f3c39e477fb6b98d710d9d298bfec7c8dfc589bed0e5cef`;
its build script SHA-256 is
`d1e3edaaa8d404a6fe311b9128063594f84ad17bf4b0fc742aad750cae95731b`.
