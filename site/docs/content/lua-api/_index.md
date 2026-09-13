+++
title = "Lua API"
weight = 6
[extra]
group = "Reference"
+++

{% raw %}
# Lua API

n00n plugins are plain Lua files. Everything a plugin can touch lives under
one global table: `n00n`. This reference documents every module, function,
and method. It is generated straight from the source code by `n00n-docgen`.

The API tries to mirror Neovim as much as possible (`n00n.fs`, `n00n.uv`,
`n00n.treesitter`, `n00n.keymap`, `n00n.base64`), signatures are kept identical
so code can be copy-pasted between the two without too many modifications.

Plugins run compiled to native code (Luau JIT). If you are debugging a
plugin and want full backtraces, start n00n with `--no-jit`: it runs your
Lua on the interpreter with complete debug info instead.

A small plugin looks like this:

```lua
n00n.api.register_command({
  name = "greet",
  description = "Say hello from Lua",
  handler = function()
    n00n.ui.flash("hello from a plugin!")
  end,
})
```

## How to read this reference

Signatures use Neovim notation: `{path}` is a required argument, `{opts?}`
is optional, and `{...}` is variadic.

One convention to remember: fallible runtime operations return a
`(value, err)` pair instead of throwing. Check `err` before using `value`:

```lua
local text, err = n00n.fs.read("config.json")
if err then
  n00n.log.error("read failed: " .. err)
  return
end
```

Lua errors are reserved for programmer mistakes, like passing a number where
a string belongs.

## Overview

| Module | What it is for |
| --- | --- |
| [`n00n`](#n00n) | The global entry point. |
| [`n00n.api`](#n00n-api) | Plugin registration. |
| [`n00n.agent`](#n00n-agent) | Subagent primitives for plugins that need to talk to an LLM. |
| [`n00n.agent.Session`](#n00n-agent-Session) | A subagent session with its own conversation history. |
| [`n00n.async`](#n00n-async) | Tools for running things concurrently in Lua plugins. |
| [`n00n.async.Semaphore`](#n00n-async-Semaphore) | A counting semaphore for limiting how many tasks run at once. |
| [`n00n.async.Permit`](#n00n-async-Permit) | One slot in a semaphore, obtained from `Semaphore:acquire()`. |
| [`n00n.base64`](#n00n-base64) | Base64 encoding and decoding, modelled after `vim.base64`. |
| [`n00n.env`](#n00n-env) | Paths to n00n's own directories (config, state, logs). |
| [`n00n.fn`](#n00n-fn) | Process and environment helpers, modeled after Neovim's `vim.fn` job |
| [`n00n.fs`](#n00n-fs) | File-system utilities, modelled after `vim.fs` and `vim.uv`. |
| [`n00n.image`](#n00n-image) | Small building blocks for working with images: probe metadata, decode |
| [`n00n.image.Image`](#n00n-image-Image) | A decoded image you can inspect, resize, crop, and re-encode. |
| [`n00n.interpreter`](#n00n-interpreter) | Run Python code in a memory-safe, time-limited sandbox. |
| [`n00n.json`](#n00n-json) | JSON encoding, decoding, schema validation, and TOON round-trip. |
| [`n00n.json.SchemaValidator`](#n00n-json-SchemaValidator) | A compiled JSON Schema validator. |
| [`n00n.keymap`](#n00n-keymap) | Key mappings, modeled after `vim.keymap`. |
| [`n00n.log`](#n00n-log) | Structured logging for plugins. |
| [`n00n.net`](#n00n-net) | HTTP client for fetching web content. |
| [`n00n.search`](#n00n-search) | Native, keyless extraction of bounded public web content. |
| [`n00n.session`](#n00n-session) | Host session primitives. |
| [`n00n.text`](#n00n-text) | Text transformation utilities. |
| [`n00n.treesitter`](#n00n-treesitter) | Tree-sitter parsing and query API. |
| [`n00n.treesitter.language`](#n00n-treesitter-language) | Language registry for tree-sitter grammars. |
| [`n00n.treesitter.query`](#n00n-treesitter-query) | Query compilation and lookup. |
| [`n00n.treesitter.Query`](#n00n-treesitter-Query) | A compiled tree-sitter query. |
| [`n00n.treesitter.Tree`](#n00n-treesitter-Tree) | A parsed syntax tree. |
| [`n00n.treesitter.Node`](#n00n-treesitter-Node) | A single node in a parsed syntax tree. |
| [`n00n.treesitter.LanguageTree`](#n00n-treesitter-LanguageTree) | Manages parsing of a source string for a single language. |
| [`n00n.ui`](#n00n-ui) | Functions for building interactive UI. |
| [`n00n.ui.Win`](#n00n-ui-Win) | Handle to a floating or split window. |
| [`n00n.ui.Buf`](#n00n-ui-Buf) | A content buffer that holds styled lines of text. |
| [`n00n.uv`](#n00n-uv) | System and environment utilities, modelled after `vim.uv`. |
| [`n00n.codegraph`](#n00n-codegraph) | Cross-file structural exploration via native `.codegraph/codegraph.db` queries with CLI fallback. |
| [`n00n.git`](#n00n-git) | In-process access to the git operations linked into n00n. |
| [`n00n.github`](#n00n-github) | GitHub REST API client using reqwest. |
| [`n00n.semblem`](#n00n-semblem) | BM25 code search and related-chunk lookup via the native `.n00n/search/` index. |
| [`n00n.smell`](#n00n-smell) | Persistent code-smell and comment index built into n00n. |
| [`n00n.workflow`](#n00n-workflow) | Sandboxed workflow script compilation. |
| [`n00n.yaml`](#n00n-yaml) | YAML encoding and decoding. |

## n00n {#n00n}

The global entry point. Every API lives under this table.

---

### `n00n.setup()` {#n00n-setup}

```lua
n00n.setup({config})
```

Apply your personal configuration. This is only available inside `init.lua` (not in plugins) and can be called at most once. The table accepts the same keys as the Configuration reference.

**Parameters:**

- `{config}` (`table`) Configuration table.

**Example:**

```lua
n00n.setup({
model = "opus",
keymaps = false,
})
```

---

### `n00n.split()` {#n00n-split}

```lua
n00n.split({s}, {sep}, {opts?})
```

Split {s} at each occurrence of {sep} and return the pieces as a
list. Mirrors Neovim's `vim.split`, so code using it can be copied
between Neovim and n00n. {sep} is a Lua pattern unless `plain` is
set; an empty {sep} splits into single characters.

**Parameters:**

- `{s}` (`string`) String to split.
- `{sep}` (`string`) Separator: a Lua pattern, or literal text with `plain`.
- `{opts?}` (`table?`) Optional settings:
  - `plain` (`boolean?`) treat {sep} as literal text instead of a pattern.
  - `trimempty` (`boolean?`) drop empty pieces from the start and end of the result.

**Returns:** (`table`) List of split pieces.

**Example:**

```lua
n00n.split("a,b,c", ",")                   -- { "a", "b", "c" }
n00n.split("x*y*z", "*", { plain = true }) -- { "x", "y", "z" }
n00n.split("\nhello\nworld\n", "\n", { trimempty = true }) -- { "hello", "world" }
```

---

### `n00n.defer_fn()` {#n00n-defer_fn}

```lua
n00n.defer_fn({callback}, {delay_ms})
```

Run {callback} after {delay_ms} without spawning a process.
Mirrors Neovim's `vim.defer_fn(fn, timeout)`.

The timer belongs to the current tool call and is cancelled when that call
ends. Use this from tool handlers; a timer scheduled by a plugin-owned
callback does not outlive that callback's task scope.

**Parameters:**

- `{callback}` (`function`) Called with the timer id and exit code `0` after the delay, or `-1` when cancelled by `jobstop`.
- `{delay_ms}` (`integer`) Delay in milliseconds.

**Returns:** (`integer`) Timer job id accepted by `n00n.fn.jobstop`.

**Example:**

```lua
n00n.defer_fn(function(timer_id, code) refresh() end, 1000)
```


## n00n.api {#n00n-api}

Plugin registration. This is where you tell n00n about your tools,
slash commands, and prompt contributions.

Most plugins only need `register_tool` and maybe `register_prompt_hint`.
Call these at the top level of your plugin file (during load).

```lua
n00n.api.register_tool({ name = "greet", ... })
n00n.api.register_prompt_hint({ slot = "tool_usage", content = "..." })
```

---

### `n00n.api.register_tool()` {#n00n-api-register_tool}

```lua
n00n.api.register_tool({spec})
```

Register a new tool the agent can call. This is the main way plugins add
capabilities to the agent. The tool is queued during plugin load and
committed to the registry once the plugin finishes loading.

Your {spec} table must include a name, a description (the model reads it
to decide when to use the tool), a JSON Schema for the input, and a handler
function. The handler receives `(input, ctx)` and returns either a plain
string or a table with richer output fields.

`cost` and `usage` are accounting metadata. n00n exposes only a finite,
non-negative cost and the allowlisted numeric token counts. If
`is_error = true` after usage was charged, this sanitized metadata is
returned alongside the error; provider payloads and extra usage fields are
discarded.

**Parameters:**

- `{spec}` (`table`) Tool specification:
  - `name` (`string`) Required. ASCII identifier, up to 64 chars ([a-zA-Z_][a-zA-Z0-9_]*).
  - `description` (`string`) Required. Non-empty description shown to the model.
  - `schema` (`table`) Required. JSON Schema object describing the tool's input parameters.
  - `handler` (`function`) Required. Called with `(input, ctx)` when the tool is invoked.
    Must return a string or a table with any of these fields:
    - `llm_output` (`string`) Text sent to the model.
    - `is_error` (`boolean`) When true, the result is treated as an error.
    - `content` (`string`) Alias for llm_output (legacy).
    - `body` (`BufHandle`) Rich rendered body shown in the UI.
    - `header` (`BufHandle`) One-line header shown before the body.
    - `format` (`string`) "plain" (default) or "markdown".
    - `annotation` (`string`) Short label shown next to the tool call.
    - `written_path` (`string`) Path of a file written by the tool.
    - `diff_path` (`string`) Path for a diff output block.
    - `diff_before` (`string`) Before text of the diff.
    - `diff_after` (`string`) After text of the diff.
    - `image` (`table`) { media_type: string, data: string } base64 image.
    - `instructions` (`table`) Array of { path, content } blocks injected as context.
    - `state` (`any`) Serializable state forwarded to restore.
    - `cost` (`number`) Non-negative estimated cost attached as sanitized tool telemetry.
    - `usage` (`table`) Token counts: fresh_input_tokens, cache_read_tokens, cache_write_tokens, input_tokens, output_tokens.
  - `audiences` (`string[]`) Which model audiences see the tool. Values: "main", "sub", "all". Default: all audiences.
  - `kind` (`string`) Optional grouping label (e.g. "filesystem").
  - `timeout` (`number`) Execution timeout in seconds. 0 or false disables. Default: inherits agent deadline.
  - `header` (`function`) Optional. Called before execution, returns a string or BufHandle for the one-line header.
  - `restore` (`function`) Optional. Called to re-render a previous tool result. Receives `(tool_name, input, output, ctx)`.
  - `start` (`function`) Optional. Called when the tool call starts, before the handler runs.
  - `describe` (`function`) Optional. Returns a custom description string for the current context.
  - `examples` (`table`) Optional. Array of example input objects for documentation.
  - `permission_scopes` (`string|function`) Field name in schema (string) or `function(input)` returning a list of path scopes that need write permission.
  - `mutable_path` (`string`) Schema field name (type: string) for the primary path the tool writes.
  - `start_annotation` (`string|table`) Schema field used to annotate the start header with a count (string) or timeout (`{ field, kind="timeout" }`).
  - `deadline_grace` (`boolean`) Optional. When true, dispatch even if the shared agent deadline
    is already exhausted, granting a brief grace window so the
    handler can settle its own state gracefully. Default: false.

**Example:**

```lua
n00n.api.register_tool({
  name = "word_count",
  description = "Count words in a file.",
  kind = "read",
  schema = {
    properties = { path = { type = "string", description = "File path" } },
    required = { "path" },
  },
  handler = function(input)
    local f = io.open(input.path, "r")
    if not f then return { llm_output = "file not found", is_error = true } end
    local n = 0
    for _ in f:read("*a"):gmatch("%S+") do n = n + 1 end
    f:close()
    return tostring(n) .. " words"
  end,
})
```

---

### `n00n.api.register_command()` {#n00n-api-register_command}

```lua
n00n.api.register_command({spec})
```

Register a slash-command that appears in the user input bar.

Slash commands let the user trigger plugin actions by typing `/name` in the
input. Use them for interactive workflows that do not need the model, like
browsing memory files or toggling settings.

**Parameters:**

- `{spec}` (`table`) Command specification:
  - `name` (`string`) Required. The command name (without the leading slash).
  - `description` (`string`) Optional. Short description shown in the command palette.
  - `handler` (`function`) Required. Called when the user runs the command.
  - `max_args` (`integer`) Optional. Maximum number of arguments the command accepts.
    Default 0 (no arguments). -1 means unlimited.

**Example:**

```lua
n00n.api.register_command({
  name = "/hello",
  description = "Say hello",
  handler = function()
    n00n.ui.flash("Hello from my plugin!")
  end,
})
```

---

### `n00n.api.register_prompt_hint()` {#n00n-api-register_prompt_hint}

```lua
n00n.api.register_prompt_hint({spec})
```

Add a piece of text to an aggregate prompt slot. Multiple plugins can each
contribute to the same slot, and all contributions are concatenated.

Good for things like tool usage guidelines or extra context that should
appear alongside other plugins' hints. If you need to own the whole slot
(e.g. identity or tone), use `set_prompt` instead.

Throws if you pass a singleton slot name.

**Parameters:**

- `{spec}` (`table`) Hint specification:
  - `slot` (`string`) Required. Aggregate slot name (e.g. "tool_usage", "general").
  - `content` (`string|function`) Required. Static text, or a `function(ctx)` that returns a string. The read-only ctx exposes `state_get(scope)` and `state_owner(scope)` when collected for a session. Max 1 MiB.
  - `prompt` (`string|string[]`) Optional. Restrict to specific prompt ids (e.g. "system").

**Example:**

```lua
n00n.api.register_prompt_hint({
  slot = "tool_usage",
  content = "- Prefer **grep** over reading entire files.",
})
```

---

### `n00n.api.register_options()` {#n00n-api-register_options}

```lua
n00n.api.register_options({spec})
```

Declare the options your plugin accepts under `plugins.<name>` in
`n00n.setup`, and get back what the user set merged with your defaults.
Call it once, at the top level of your plugin file.

An unknown key, a wrong type, or a value below `min` fails the plugin
load with a clear message, so users catch typos right away. Bad specs
fail the load too. The specs also feed the generated configuration docs.

**Parameters:**

- `{spec}` (`table`) Map of option name to a spec table:
  - `default` (`boolean|number|string`) Optional. Used when the user sets nothing. Its Lua type becomes the option type.
  - `type` (`string`) Required when there is no default: "boolean", "integer", "number", or "string".
  - `min` (`number`) Optional. Minimum accepted value, numeric options only.
  - `desc` (`string`) Required. One line shown in the configuration docs.

**Returns:** (`table`) Merged options: the user's value where set, otherwise the default, or nil when neither exists.

**Example:**

```lua
local opts = n00n.api.register_options({
  timeout_secs = { default = 120, min = 5, desc = "Kill the command after this many seconds." },
  max_output_lines = { type = "integer", desc = "Override agent.max_output_lines for this tool." },
})
```

---

### `n00n.api.set_prompt()` {#n00n-api-set_prompt}

```lua
n00n.api.set_prompt({spec})
```

Set a singleton prompt slot. Only one plugin owns each singleton slot at a
time, so calling this replaces any previous value from your plugin.

Use this for slots like "identity" or "tone" where a single coherent value
makes more sense than combining fragments. For aggregate slots like
"tool_usage", use `register_prompt_hint` instead.

Throws if you pass an aggregate slot name.

**Parameters:**

- `{spec}` (`table`) Spec fields mirror `register_prompt_hint`:
  - `slot` (`string`) Required. Singleton slot name (e.g. "identity", "tone").
  - `content` (`string|function`) Required. Static text or a `function(ctx)` returning a string. The read-only ctx exposes `state_get(scope)` and `state_owner(scope)` when collected for a session. Max 1 MiB.
  - `prompt` (`string|string[]`) Optional. Restrict to specific prompt ids.

**Example:**

```lua
n00n.api.set_prompt({
  slot = "tone",
  content = "Be concise. No filler words.",
})
```

---

### `n00n.api.get_tools()` {#n00n-api-get_tools}

```lua
n00n.api.get_tools({opts?})
```

Return a list of all registered tools. Useful for building UI that shows
available tools or for checking which tools are enabled.

Each entry has the tool's name, schema, audiences, deferred-loading state,
and an `enabled` flag.
Describe callbacks are not invoked (the static description is used).

**Parameters:**

- `{opts?}` (`table?`) Options:
  - `config` (`table`) Optional config table with a `disabled_tools` string[] field used to compute the `enabled` flag on each entry.

**Returns:** (`table[]`) Array of tool entries: { name, schema, audiences, deferred, kind?, enabled }.

**Example:**

```lua
local tools = n00n.api.get_tools()
for _, t in ipairs(tools) do
  print(t.name, t.enabled)
end
```

---

### `n00n.api.get_tool()` {#n00n-api-get_tool}

```lua
n00n.api.get_tool({name})
```

Look up a single tool by name. Returns its metadata table or nil if the
tool does not exist. For Lua-registered tools the returned table also
includes `header` and `restore` handle functions (wrapped so they never
throw).

**Parameters:**

- `{name}` (`string`) Exact tool name.

**Returns:** (`table|nil`) Tool entry with fields { name, schema, audiences, deferred, kind?, header?, restore? }, or nil if not found.

**Example:**

```lua
local t = n00n.api.get_tool("bash")
if t then
  print("bash audiences:", table.concat(t.audiences, ", "))
end
```

---

### `n00n.api.create_autocmd()` {#n00n-api-create_autocmd}

```lua
n00n.api.create_autocmd({event}, {opts})
```

Listen for one or more events. Returns an id you can pass to
`del_autocmd` later to remove the listener.

Built-in events fired by the host: `"TurnStart"`, `"TurnEnd"`,
`"TurnError"`, `"ToolStart"`, `"ToolDone"`, `"SessionReset"`,
`"SessionFocus"`.
Plugins can also fire their own
events with `exec_autocmds`.

**Parameters:**

- `{event}` (`string|string[]`) Event name or list of names.
- `{opts}` (`table`) Options:
  - `callback` (`function`) called with an ev table `{ id, event, match, data }`.
  - `once` (`boolean`) remove the handler after it fires once (default false).
  - `pattern` (`string|string[]`) only fire when the pattern matches. `"*"` matches everything. Omit to match all.

**Returns:** (`integer`) Autocmd id.

**Example:**

```lua
local id = n00n.api.create_autocmd("TurnEnd", {
  callback = function(ev)
    print("turn ended: " .. ev.event)
  end,
})
```

---

### `n00n.api.del_autocmd()` {#n00n-api-del_autocmd}

```lua
n00n.api.del_autocmd({id})
```

Remove a previously registered autocmd. Does nothing if the {id}
does not exist.

**Parameters:**

- `{id}` (`integer`) Id returned by `create_autocmd`.

**Example:**

```lua
n00n.api.del_autocmd(id)
```

---

### `n00n.api.exec_autocmds()` {#n00n-api-exec_autocmds}

```lua
n00n.api.exec_autocmds({event}, {opts?})
```

Fire one or more events manually. Every matching autocmd callback
runs synchronously before this function returns.

**Parameters:**

- `{event}` (`string|string[]`) Event name or list of names to fire.
- `{opts?}` (`table?`) Options:
  - `pattern` (`string`) passed to callbacks as `ev.match`.
  - `data` (`any`) arbitrary value passed as `ev.data`.

**Example:**

```lua
n00n.api.exec_autocmds("MyEvent", {
  pattern = "init",
  data = { msg = "hello" },
})
```

---

### `n00n.api.declare_slot()` {#n00n-api-declare_slot}

```lua
n00n.api.declare_slot({name}, {default})
```

Create a named extension point owned by your plugin. You provide a
{default} function, and other plugins can wrap it with layers using
`set_slot`. The returned callable runs the full chain: outermost
layer first, then inward, ending at {default}.

Throws if another plugin already owns a slot with the same {name}.

**Parameters:**

- `{name}` (`string`) Unique slot name, e.g. `"myplugin.render"`.
- `{default}` (`function`) Default implementation, called when no layers wrap it.

**Returns:** (`function`) Callable that dispatches through all layers.

**Example:**

```lua
local render = n00n.api.declare_slot("myplugin.render", function(text)
  return text:upper()
end)
print(render("hello")) -- HELLO
```

---

### `n00n.api.set_slot()` {#n00n-api-set_slot}

```lua
n00n.api.set_slot({name}, {wrapper})
```

Add a layer around an existing (or future) slot. Layers wrap the
default from the outside in. Each layer receives `prev` as its
first argument. Call `prev(...)` to continue down the chain.
Calling `prev` more than once throws.

You can call this before the owner runs `declare_slot`. The layer
is queued and attached when the slot is declared.

**Parameters:**

- `{name}` (`string`) Slot name to wrap.
- `{wrapper}` (`function`) Layer: `function(prev, ...)`. Call `prev(...)` to continue.

**Example:**

```lua
n00n.api.set_slot("myplugin.render", function(prev, text)
  return prev("[" .. text .. "]")
end)
```

---

### `n00n.api.get_slots()` {#n00n-api-get_slots}

```lua
n00n.api.get_slots()
```

List all known slots and their current state. Useful for debugging
which plugins own or wrap each slot.

**Returns:** (`table`) Map of slot name to `{ owner, declared, fillers }`.

**Example:**

```lua
for name, info in pairs(n00n.api.get_slots()) do
  print(name, info.owner, info.declared)
end
```


## n00n.agent {#n00n-agent}

Subagent primitives for plugins that need to talk to an LLM.

This module gives you the building blocks: resolve which model to use,
build a system prompt, list available tools, call a tool directly, or
open a full session with its own conversation history.

Policy like retries, validation, and concurrency lives in the calling
plugin, not here.

```lua
local tools = n00n.agent.tools(ctx, { audience = "general_sub" })
local sess = n00n.agent.session(ctx, {
  system = "You are a helpful assistant.",
  tools = tools,
})
local r = sess:prompt("Hello!")
print(r.text)
sess:close()
```

---

### `n00n.agent.resolve_model()` {#n00n-agent-resolve_model}

```lua
n00n.agent.resolve_model({ctx}, {opts?})
```

Look up the model that the current agent is using, or pick a cheaper one.
You might want a cheaper model for simple subtasks (summaries, classification)
without hard-coding a model name.

The returned table has fields: `id` (string), `tier` (string),
`provider` (string), `spec` (string).

**Parameters:**

- `{ctx}` (`LuaCtx`) Agent context.
- `{opts?}` (`table?`) Optional fields:
  - `tier` (`string?`) target tier, e.g. `"fast"`, `"mid"`, `"best"`. Clamped to
    the parent tier so you cannot escalate.
  - `spec` (`string?`) exact model spec string, e.g. `"claude-3-5-haiku-20241022"`.
    Takes precedence over `tier`.

**Returns:** (`table?`, `string?`) Model table on success, or `(nil, err)` on failure.

**Example:**

```lua
local model, err = n00n.agent.resolve_model(ctx, { tier = "fast" })
if err then error(err) end
print(model.spec, model.tier)
```

---

### `n00n.agent.system_prompt()` {#n00n-agent-system_prompt}

```lua
n00n.agent.system_prompt({ctx}, {opts})
```

Build a system prompt from a built-in template. Environment variables like
`{cwd}` are substituted automatically. Use this when you need a ready-made
prompt for a subagent session.

**Parameters:**

- `{ctx}` (`LuaCtx`) Agent context.
- `{opts}` (`table`) Required fields:
  - `prompt_id` (`string`) one of `"research"`, `"general"`, `"system"`.

  Optional fields:

  - `instructions` (`string|boolean?`) extra text appended to the prompt.
    `true` loads instructions from the project `.n00n/instructions` file.
    `false` or nil omits them.

**Returns:** (`string?`, `string?`) The assembled prompt string, or `(nil, err)` on failure.

**Example:**

```lua
local prompt, err = n00n.agent.system_prompt(ctx, {
  prompt_id = "research",
  instructions = true,
})
if err then error(err) end
```

---

### `n00n.agent.tools()` {#n00n-agent-tools}

```lua
n00n.agent.tools({ctx}, {opts})
```

Get the list of tool definitions for a given audience. Pass the result
straight into `n00n.agent.session()` or use it to inspect what tools are
available.

**Parameters:**

- `{ctx}` (`LuaCtx`) Agent context.
- `{opts}` (`table`) Required fields:
  - `audience` (`string`) tool audience filter, e.g. `"general"`, `"subagent"`,
    `"general_sub"`.

  Optional fields:

  - `only` (`string[]?`) include only these tool names.
  - `except` (`string[]?`) exclude these tool names.
  - `include_mcp` (`boolean?`) include MCP tools. Default: `true`.
  - `workflow` (`boolean?`) use workflow-mode descriptions. Default: `false`.
  - `spec` (`string?`) evaluate capability exclusions against this model spec.

**Returns:** (`table?`, `string?`) Array of tool definition tables, or `(nil, err)` on failure.

**Example:**

```lua
local defs, err = n00n.agent.tools(ctx, {
  audience = "general_sub",
  except = { "bash", "write" },
})
if err then error(err) end
print(#defs .. " tools available")
```

---

### `n00n.agent.call_tool()` {#n00n-agent-call_tool}

```lua
n00n.agent.call_tool({ctx}, {name}, {input}, {opts?})
```

Run a tool by name and wait for the result. This is how you call built-in
tools (like `read`, `bash`, `glob`) from Lua without going through the LLM.

Live events (streaming output, annotations) are delivered through optional
callbacks while the tool runs.

**Parameters:**

- `{ctx}` (`LuaCtx`) Agent context.
- `{name}` (`string`) Tool name, e.g. `"bash"`, `"read"`.
- `{input}` (`table|any`) Tool input (JSON-serializable). Must match the tool's `input_schema`.
- `{opts?}` (`table?`) Optional fields:
  - `timeout` (`integer?`) deadline in seconds.
  - `on_live_buf` (`function?`) called with a `BufHandle` for each live buffer
    the tool publishes. Must not yield.
  - `on_annotation` (`function?`) called with an annotation string for each
    annotation event. Must not yield.

**Returns:** (`string?`, `string?`) Tool output text, or `(nil, err)` on failure.

**Example:**

```lua
local out, err = n00n.agent.call_tool(ctx, "bash", {
  command = "ls -la",
  timeout = 10,
})
if err then error(err) end
print(out)
```

---

### `n00n.agent.session()` {#n00n-agent-session}

```lua
n00n.agent.session({ctx}, {opts})
```

Create a new subagent session. The session inherits the parent model and
MCP handle unless you override them. You get back a `Session` object that
you can send messages to with `:prompt()`.

This is the main way to spin up a sub-conversation with its own history
and tool set.

**Parameters:**

- `{ctx}` (`LuaCtx`) Agent context.
- `{opts}` (`table`) Optional fields:
  - `model_spec` (`string?`) model spec string to use instead of the parent model.
  - `system` (`string?`) system prompt. Defaults to empty.
  - `tools` (`table?`) tool definitions array (from `n00n.agent.tools()`).
  - `local_tools` (`table?`) map of `name -> spec` for Lua-backed tools. Each spec
    requires `description` (string), `input_schema` (table), and
    `handler` (function). The handler receives the input table and must return
    `(string)` or `(nil, err)`.
  - `name` (`string?`) display name for logs and UI.
  - `audience` (`string?`) tool audience for capability gating. Default: `"general_sub"`.
  - `mode` (`string?`) agent operating mode: `"build"` (default), `"research"`, `"plan"`, or the

  alias `"general"` (build). Plan mode requires `plan_path`.

  - `plan_path` (`string?`) required when `mode` is `"plan"`; path to the approved plan file.
  - `thinking` (`string|integer?`) thinking mode: `"off"`, `"adaptive"`, an
    effort level (`"minimal"`, `"low"`, `"medium"`, `"high"`, `"xhigh"`,
    `"max"`), or a budget integer (token count). Inherits parent setting
    if omitted.
  - `fast` (`boolean?`) use fast mode. Inherits parent setting if omitted.
  - `include_mcp` (`boolean?`) inherit the parent MCP handle. Default: `true`.
  - `except` (`string[]?`) tool names that remain unavailable if loaded later.

**Returns:** ([`Session?`](#n00n-agent-Session), `string?`) Session handle, or `(nil, err)` on failure.

**Example:**

```lua
local tools = n00n.agent.tools(ctx, { audience = "general_sub" })
local sess, err = n00n.agent.session(ctx, {
  system = "You are a research assistant.",
  tools = tools,
  name = "researcher",
})
if err then error(err) end
local result = sess:prompt("Summarize this file.")
sess:close()
```

---

### `n00n.agent.usage_cost()` {#n00n-agent-usage_cost}

```lua
n00n.agent.usage_cost({spec}, {input_tokens}, {output_tokens}, {breakdown?})
```

Estimate the dollar cost of a completion from its model spec and token
counts. Uses the provider's published pricing for fresh input, cache reads,
cache writes, and output. Without {breakdown}, input and fast-tier pricing
retain the legacy three-argument behavior.

**Parameters:**

- `{spec}` (`string`) Model spec, e.g. `"anthropic/claude-haiku-4-5"`.
- `{input_tokens}` (`integer`) Total prompt tokens across all input categories.
- `{output_tokens}` (`integer`) Completion tokens.
- `{breakdown?}` (`table?`) Optional input categories. Fields:
  - `fresh_input_tokens` (`integer?`) non-cached input; when present, it must
    conserve `input_tokens` together with the cache categories.
  - `cache_read_tokens` (`integer?`) input tokens read from cache; default 0.
  - `cache_write_tokens` (`integer?`) input tokens written to cache; default 0.
  - `fast` (`boolean?`) whether this completion used fast-tier pricing; default false.

**Returns:** (`number?`, `string?`) Estimated USD cost, or `(nil, err)` on failure.

**Example:**

```lua
local cost, err = n00n.agent.usage_cost("anthropic/claude-haiku-4-5", 1200, 300, {
  fresh_input_tokens = 900,
  cache_read_tokens = 200,
  cache_write_tokens = 100,
})
if err then error(err) end
print(string.format("$%.4f", cost))
```


## n00n.agent.Session {#n00n-agent-Session}

A subagent session with its own conversation history.

Create one with `n00n.agent.session()`, then send messages with
`:prompt()`. The session remembers previous turns, so you can have
a multi-step conversation. Call `:close()` when you are done, or let
garbage collection handle it.

---

### `Session:prompt()` {#Session-prompt}

```lua
Session:prompt({message})
```

Send a message to the subagent and wait for its full response. The agent
loop runs to completion, calling tools as needed. Conversation history is
kept across calls, so you can have a multi-turn conversation.

The success table has fields: `text` (string), `duration_ms` (integer),
`input_tokens` (integer), `fresh_input_tokens` (integer),
`cache_read_tokens` (integer), `cache_write_tokens` (integer),
`output_tokens` (integer), actual `fast` state (boolean), and aggregate
`cost` (number). The cost is summed
per request using that request's model and fast tier, including compaction.
If execution fails after incurring usage, the
result table omits `text` and contains only `duration_ms` plus these
sanitized numeric usage fields. Check `err` before reading `text`.

**Parameters:**

- `{message}` (`string`) User message to send.

**Returns:** (`table?`, `string?`) `(result, nil)` on success; charged failures return
  `(sanitized_usage, err)`. Session-state failures can return `(nil, err)`.

**Example:**

```lua
local r, err = sess:prompt("What files are in this project?")
if err then error(err) end
print(r.text)
print(r.input_tokens .. " input, " .. r.output_tokens .. " output tokens")
```

---

### `Session:close()` {#Session-close}

```lua
Session:close()
```

Close the session and flush its history back to the parent agent. You can
call this multiple times safely. If you forget, it runs automatically when
the session is garbage collected.

---

### `Session:get_progress()` {#Session-get_progress}

```lua
Session:get_progress()
```

Poll the session for a progress snapshot while a prompt is running.

Returns a table with:
  `elapsed_ms` (integer): time since the session was created.
  `current_tool` (string?): name of the tool currently running, if any.
  `recent_tools` (table): names of the last few finished tools, oldest first.
  `activities` (table): up to five safe rendered tool summaries, oldest first.
  `completed_count` (integer): total number of finished tools so far.
  `turn_id` (integer): increases before each `prompt` call.
  `done` (bool): true once the current prompt call has completed.

The call returns at most every `PROGRESS_TIMEOUT_MS` milliseconds, or
immediately when a tool starts or finishes.

---

### `Session:cancel()` {#Session-cancel}

```lua
Session:cancel()
```

Cancel the current turn in this session without closing it. The agent will
stop at the next cancellation point and return an error from `:prompt()`.


## n00n.async {#n00n-async}

Tools for running things concurrently in Lua plugins.

Use `run` to fire off background tasks, `gather` or `join` to run
several functions at once, and `semaphore` to limit concurrency.
The `await` and `wrap` helpers bridge callback-based APIs into
coroutine-friendly calls.

```lua
local results = n00n.async.gather({
  function() return fetch("a.txt") end,
  function() return fetch("b.txt") end,
})
```

---

### `n00n.async.run()` {#n00n-async-run}

```lua
n00n.async.run({fn}, {on_finish?})
```

Fire off a function as a new async task. It runs in the background and
you do not wait for it. If you need the result, pass an {on_finish}
callback. A bounded number of `async.run` tasks may be queued or running at
once; excess fanout returns an error instead of consuming memory without bound.

**Parameters:**

- `{fn}` (`function`) Zero-argument function to execute.
- `{on_finish?}` (`function?`) Optional callback `function(err, result)`. Called once {fn} completes.

**Example:**

```lua
n00n.async.run(function()
  local data = expensive_fetch()
  process(data)
end)
```

---

### `n00n.async.await()` {#n00n-async-await}

```lua
n00n.async.await({argc}, {fn}, {...})
```

Turn a callback-based function into a normal call you can use in a coroutine. It calls `fn(..., callback)`, inserting the callback at position {argc}, then suspends your coroutine until the callback fires. You get back whatever the callback was called with.

**Parameters:**

- `{argc}` (`integer`) Total number of positional arguments {fn} expects (including the callback). Must be >= 1.
- `{fn}` (`function`) Callback-based function to call.
- `{...}` (`any`) Extra arguments forwarded to {fn} before the injected callback.

**Returns:** (`...`) Values passed by the caller to the injected callback.

**Example:**

```lua
local result = n00n.async.await(2, http.get, url)
```

---

### `n00n.async.wrap()` {#n00n-async-wrap}

```lua
n00n.async.wrap({argc}, {fn})
```

Create a coroutine-friendly wrapper around a callback-based function. The wrapper calls `n00n.async.await` for you, so you can use the result like a normal function call.

**Parameters:**

- `{argc}` (`integer`) Callback position, forwarded to `n00n.async.await`.
- `{fn}` (`function`) Callback-based function to wrap.

**Returns:** (`function`) Wrapped function you can call like a normal function.

**Example:**

```lua
local get = n00n.async.wrap(2, http.get)
local body = get(url)
```

---

### `n00n.async.join()` {#n00n-async-join}

```lua
n00n.async.join({max_jobs}, {fns})
```

Run all functions in {fns} with at most {max_jobs} going at once. Waits until every function has finished. Unlike `gather`, this does not return individual results.

**Parameters:**

- `{max_jobs}` (`integer`) Maximum number of functions running at the same time.
- `{fns}` (`table`) Array of zero-argument functions to execute.

**Example:**

```lua
n00n.async.join(4, {
  function() process(files[1]) end,
  function() process(files[2]) end,
  function() process(files[3]) end,
})
```

---

### `n00n.async.gather()` {#n00n-async-gather}

```lua
n00n.async.gather({fns})
```

Run all functions in {fns} at the same time and collect their results.
Unlike `join`, this gives you back the return value (or error) from each
function. The results are in the same order as the input.

Each entry in the result array has `ok` (boolean), and either `value`
(on success) or `err` (string, on failure).

**Parameters:**

- `{fns}` (`table`) Array of zero-argument functions.

**Returns:** (`table`) Array of result tables, one per function.

**Example:**

```lua
local results = n00n.async.gather({
  function() return fetch("a.txt") end,
  function() return fetch("b.txt") end,
})
for i, r in ipairs(results) do
  if r.ok then print(r.value) else print("error: " .. r.err) end
end
```

---

### `n00n.async.semaphore()` {#n00n-async-semaphore}

```lua
n00n.async.semaphore({n})
```

Create a counting semaphore that allows at most {n} concurrent permits.
Use this to limit how many tasks hit a resource at the same time.

**Parameters:**

- `{n}` (`integer`) Maximum number of concurrent permits. Values below 1 are clamped to 1.

**Returns:** ([`n00n.async.Semaphore`](#n00n-async-Semaphore)) A new semaphore.

**Example:**

```lua
local sem = n00n.async.semaphore(5)
-- each task acquires a permit before doing work
local permit = sem:acquire()
do_work()
permit:release()
```


## n00n.async.Semaphore {#n00n-async-Semaphore}

A counting semaphore for limiting how many tasks run at once.

Create one with `n00n.async.semaphore(n)`, then call `:acquire()` to
get a permit before doing work. If the task is cancelled, the acquire
is cancelled too.

---

### `Semaphore:acquire()` {#Semaphore-acquire}

```lua
Semaphore:acquire()
```

Wait for a permit from the semaphore. Your coroutine suspends until a slot
opens up. If the owning task is cancelled, the acquire is cancelled too.

**Returns:** ([`n00n.async.Permit`](#n00n-async-Permit)) A permit handle. Call `:release()` when done, or let it be garbage collected.

**Example:**

```lua
local sem = n00n.async.semaphore(3)
local permit = sem:acquire()
-- do work that needs the slot
permit:release()
```


## n00n.async.Permit {#n00n-async-Permit}

One slot in a semaphore, obtained from `Semaphore:acquire()`.

The slot is held until you call `:release()` or until the permit is
garbage collected. Releasing early lets other tasks acquire sooner.

---

### `Permit:release()` {#Permit-release}

```lua
Permit:release()
```

Give the permit back to the semaphore so another task can acquire it.
Throws if you already released this permit.


## n00n.base64 {#n00n-base64}

Base64 encoding and decoding, modelled after `vim.base64`.

Both functions accept strings and Luau buffers, so you can round-trip
binary data read with `n00n.fs.read_bytes`.

```lua
local encoded = n00n.base64.encode("hello")
local decoded = n00n.base64.decode(encoded)
```

---

### `n00n.base64.encode()` {#n00n-base64-encode}

```lua
n00n.base64.encode({data})
```

Encode {data} to standard Base64. Like `vim.base64.encode`.
Accepts both strings and Luau buffers.

**Parameters:**

- `{data}` (`string|buffer`) Data to encode.

**Returns:** (`string`) Base64-encoded string.

**Example:**

```lua
n00n.base64.encode("hello") -- "aGVsbG8="
```

---

### `n00n.base64.decode()` {#n00n-base64-decode}

```lua
n00n.base64.decode({str})
```

Decode a Base64-encoded {str} back to its original bytes. Like `vim.base64.decode`.
Throws if {str} is not valid Base64.

**Parameters:**

- `{str}` (`string|buffer`) Base64-encoded text.

**Returns:** (`string`) Decoded bytes as a string.

**Example:**

```lua
n00n.base64.decode("aGVsbG8=") -- "hello"
```


## n00n.env {#n00n-env}

Paths to n00n's own directories (config, state, logs).

Use these to locate config files or persistent state without hard-coding paths.

```lua
local cfg = n00n.env.config_dir()
```

---

### `n00n.env.state_dir()` {#n00n-env-state_dir}

```lua
n00n.env.state_dir()
```

Return the directory where n00n stores runtime state (sessions, auth tokens, etc.).
Typically something like `~/.local/state/n00n`.

**Returns:** (`string?`) State directory path, or nil if it cannot be determined.

**Example:**

```lua
local dir = n00n.env.state_dir()
```

---

### `n00n.env.config_dir()` {#n00n-env-config_dir}

```lua
n00n.env.config_dir()
```

Return the directory where n00n looks for user configuration files.
Typically something like `~/.config/n00n`.

**Returns:** (`string?`) Config directory path, or nil if it cannot be determined.

**Example:**

```lua
local dir = n00n.env.config_dir()
```

---

### `n00n.env.logs_dir()` {#n00n-env-logs_dir}

```lua
n00n.env.logs_dir()
```

Return the directory where n00n writes its log files (`n00n.log`).
Typically something like `~/.local/logs/n00n`.

**Returns:** (`string?`) Logs directory path, or nil if it cannot be determined.

**Example:**

```lua
local dir = n00n.env.logs_dir()
```


## n00n.fn {#n00n-fn}

Process and environment helpers, modeled after Neovim's `vim.fn` job
control. Use these to run shell commands, wait for output, and check
whether programs are installed.

Job functions need the `run` permission. `executable` needs the `env`
permission.

A job belongs to the call that started it unless you pass
`owner = "plugin"`, which keeps it running until the plugin unloads
or reloads. Only the owning task or plugin can stop or wait on a job.

```lua
local id = n00n.fn.jobstart("git status", {
  on_exit = function(code) print("done: " .. code) end,
})
```

---

### `n00n.fn.jobstart()` {#n00n-fn-jobstart}

```lua
n00n.fn.jobstart({cmd}, {opts?})
```

Run a shell command in the background. The command runs through
`bash -c` on Unix or `cmd /C` on Windows. You get back a job id
that you can pass to `jobstop` or `jobwait` to control the process.

For commands that don't need shell features (pipes, redirection, globs),
pass an array to run the program directly with preserved argument quoting:
`n00n.fn.jobstart({ "git", "commit", "-m", "feat: msg" })`

Unix jobs run in a separate process group at nice level 10. On Linux, the
process tree's summed per-process RSS is limited to one quarter of system
memory, clamped between 512 MiB and 8 GiB. Shared pages may be counted more
than once. Set `N00N_TOOL_MAX_RSS_MB` to a positive whole number of MiB to
override the memory limit.

**Parameters:**

- `{cmd}` (`string|table`) Shell command string, or array of program + args.
- `{opts?}` (`table?`) Optional settings:
  - `cwd` (`string?`) working directory (tilde is expanded).
  - `env` (`table?`) extra environment variables, `{ VAR = "value" }`.
  - `on_stdout` (`function?`) called with `(job_id, line)` for each stdout line.
  - `on_stderr` (`function?`) called with `(job_id, line)` for each stderr line.
  - `on_exit` (`function?`) called with `(job_id, code)` when the process finishes.
  - `owner` (`string?`) job lifetime. `"task"` (default) ends the job with
    the current call. `"plugin"` keeps it alive until the plugin unloads
    or reloads.

**Returns:** (`integer`) Job id.

**Example:**

```lua
-- String mode (shell features available)
local id = n00n.fn.jobstart("ls -la", {
  cwd = "~/projects",
  on_stdout = function(_, line) print(line) end,
  on_exit = function(_, code) print("exit: " .. code) end,
})
-- List mode (preserves argument quoting)
local id = n00n.fn.jobstart({ "git", "commit", "-m", "feat: preserve spaces" }, opts)
-- Plugin-owned watcher that outlives the call that started it
local watcher = n00n.fn.jobstart("tail -F app.log", { owner = "plugin" })
```

---

### `n00n.fn.jobstop()` {#n00n-fn-jobstop}

```lua
n00n.fn.jobstop({job_id})
```

Kill a running process immediately (SIGKILL on Unix) or cancel a deferred
timer. Safe to call on jobs that already exited or on unknown ids. A
cancelled timer's callback runs with exit code `-1`.

**Parameters:**

- `{job_id}` (`integer`) Job id returned by `jobstart` or `defer`.

**Example:**

```lua
n00n.fn.jobstop(id)
```

---

### `n00n.fn.jobwait()` {#n00n-fn-jobwait}

```lua
n00n.fn.jobwait({job_id}, {timeout_ms?})
```

Wait for a job to finish and collect its output. Returns a result
table with `stdout`, `stderr`, and `exit_code`. Returns `nil` if the
job does not finish before the timeout.

While waiting, the job's `on_stdout`, `on_stderr`, and `on_exit`
callbacks fire as events arrive (like Neovim), so you can stream
output into a buffer while parked here.

**Parameters:**

- `{job_id}` (`integer`) Job id returned by `jobstart`.
- `{timeout_ms?}` (`integer?`) Maximum wait in milliseconds (default 30000).

**Returns:** (`table?`) `{ stdout, stderr, exit_code }`, or nil on timeout.

**Example:**

```lua
local id = n00n.fn.jobstart("echo hello")
local result = n00n.fn.jobwait(id, 5000)
if result then
  print(result.stdout)
end
```

---

### `n00n.fn.defer()` {#n00n-fn-defer}

```lua
n00n.fn.defer({callback}, {delay_ms})
```

Run {callback} after {delay_ms} without spawning a process.
Prefer `n00n.defer_fn(callback, delay_ms)`, which mirrors Neovim. This
compatibility helper also accepts the previous `(delay_ms, callback)` order.

**Parameters:**

- `{callback}` (`function`) Called with the timer id and exit code `0` after the delay, or `-1` when cancelled by `jobstop`.
- `{delay_ms}` (`integer`) Delay in milliseconds.

**Returns:** (`integer`) Timer job id accepted by `jobstop`.

**Example:**

```lua
n00n.fn.defer(function(timer_id, code) refresh() end, 1000)
```

---

### `n00n.fn.executable()` {#n00n-fn-executable}

```lua
n00n.fn.executable({name})
```

Check whether {name} can be found on `$PATH` or is an absolute path
to a file. Returns 1 when found, 0 otherwise (matches Neovim's
`vim.fn.executable`).

**Parameters:**

- `{name}` (`string`) Program name (e.g. `"git"`) or absolute path.

**Returns:** (`integer`) `1` if found, `0` otherwise.

**Example:**

```lua
if n00n.fn.executable("rg") == 1 then
  -- use ripgrep
end
```


## n00n.fs {#n00n-fs}

File-system utilities, modelled after `vim.fs` and `vim.uv`.

Fallible operations return `(value, err)` pairs and never throw.
Paths support `~/` expansion. Relative paths resolve from the current working directory.

```lua
local text, err = n00n.fs.read("init.lua")
if err then return end
```

---

### `n00n.fs.read()` {#n00n-fs-read}

```lua
n00n.fs.read({path})
```

Read the entire file at {path} as a UTF-8 string.
If the file contains bytes that are not valid UTF-8, this function throws.
Use `read_bytes` for binary files.

**Parameters:**

- `{path}` (`string`) Absolute or relative file path. `~/` is expanded to the home directory.

**Returns:** (`string?`, `string?`) File contents, or nil plus an error message.

**Example:**

```lua
local text, err = n00n.fs.read("config.toml")
if err then
  n00n.log.warn("could not read config: " .. err)
  return
end
```

---

### `n00n.fs.read_bytes()` {#n00n-fs-read_bytes}

```lua
n00n.fs.read_bytes({path})
```

Read the entire file at {path} as raw bytes, returned as a Luau buffer.
Useful for binary files or when you need to pass the data to `n00n.base64.encode`.

**Parameters:**

- `{path}` (`string`) Absolute or relative file path. `~/` is expanded to the home directory.

**Returns:** (`buffer?`, `string?`) File bytes as a Luau buffer, or nil plus an error message.

**Example:**

```lua
local buf, err = n00n.fs.read_bytes("image.png")
if err then return end
local encoded = n00n.base64.encode(buf)
```

---

### `n00n.fs.read_bytes_limited()` {#n00n-fs-read_bytes_limited}

```lua
n00n.fs.read_bytes_limited({path}, {max_bytes})
```

Read at most {max_bytes} raw bytes from a regular file at {path}.
The opened handle is checked before reading, so devices, FIFOs, and directories
are rejected. Reads one byte beyond the limit to report oversized files without
allocating their full contents.

**Parameters:**

- `{path}` (`string`) Absolute or relative file path. `~/` is expanded to the home directory.
- `{max_bytes}` (`integer`) Maximum file size in bytes.

**Returns:** (`buffer?`, `string?`) File bytes, or nil plus a sanitized error message.

**Example:**

```lua
local bytes, err = n00n.fs.read_bytes_limited("image.png", 50 * 1024 * 1024)
if err then return end
```

---

### `n00n.fs.read_lines()` {#n00n-fs-read_lines}

```lua
n00n.fs.read_lines({path}, {offset}, {limit})
```

Read lines from a file at {path} with offset and limit, streaming without loading the entire file.
Returns a table with `lines` (array of strings), `total_lines` (integer), and `prefix` (string or nil).
The `prefix` contains up to 256 lines immediately before the offset for context.
Strips trailing `\r` from each line. Returns an error on non-UTF-8 content.

**Parameters:**

- `{path}` (`string`) Absolute or relative file path. `~/` is expanded to the home directory.
- `{offset}` (`integer`) 1-based line number to start reading from (default 1).
- `{limit}` (`integer`) Maximum number of lines to return (default all remaining lines).

**Returns:** (`table?`, `string?`) Result table with `lines`, `total_lines`, and `prefix`, or nil plus an error message.

**Example:**

```lua
local res, err = n00n.fs.read_lines("file.txt", 10, 50)
if err then return end
for i, line in ipairs(res.lines) do print(line) end
print("total:", res.total_lines)
```

---

### `n00n.fs.metadata()` {#n00n-fs-metadata}

```lua
n00n.fs.metadata({path})
```

Get metadata for the file or directory at {path}.
Returns a table with `size` (integer), `is_file` (boolean), and `is_dir` (boolean).
If {path} does not exist, returns nil with no error.

**Parameters:**

- `{path}` (`string`) Absolute or relative path.

**Returns:** (`table?`, `string?`) Metadata table, nil if missing, or nil plus an error message.

**Example:**

```lua
local meta = n00n.fs.metadata("src/main.rs")
if meta and meta.is_file then
  print("size: " .. meta.size)
end
```

---

### `n00n.fs.dirname()` {#n00n-fs-dirname}

```lua
n00n.fs.dirname({path})
```

Return the parent directory of {path}. Like `vim.fs.dirname`.

**Parameters:**

- `{path}` (`string`) File path.

**Returns:** (`string?`) Parent directory, or nil if {path} has no parent.

**Example:**

```lua
n00n.fs.dirname("/home/user/init.lua") -- "/home/user"
```

---

### `n00n.fs.basename()` {#n00n-fs-basename}

```lua
n00n.fs.basename({path})
```

Return the final component (the file name) of {path}. Like `vim.fs.basename`.

**Parameters:**

- `{path}` (`string`) File path.

**Returns:** (`string?`) File name, or nil for paths like `/`.

**Example:**

```lua
n00n.fs.basename("/home/user/init.lua") -- "init.lua"
```

---

### `n00n.fs.joinpath()` {#n00n-fs-joinpath}

```lua
n00n.fs.joinpath({...})
```

Join one or more path segments into a single path. Like `vim.fs.joinpath`.

**Parameters:**

- `{...}` (`string`) One or more path segments to join.

**Returns:** (`string`) The joined path.

**Example:**

```lua
n00n.fs.joinpath("src", "api", "fs.rs") -- "src/api/fs.rs"
```

---

### `n00n.fs.normalize()` {#n00n-fs-normalize}

```lua
n00n.fs.normalize({path})
```

Clean up `.` and `..` segments and make {path} absolute. Like `vim.fs.normalize`.
This is purely string-based and does not touch the filesystem.

**Parameters:**

- `{path}` (`string`) Path to normalize. `~/` is expanded.

**Returns:** (`string`) Normalized absolute path.

**Example:**

```lua
n00n.fs.normalize("src/../src/api") -- "/home/user/project/src/api"
```

---

### `n00n.fs.abspath()` {#n00n-fs-abspath}

```lua
n00n.fs.abspath({path})
```

Make {path} absolute by prepending the current working directory when needed.
Unlike `normalize`, this does not resolve `.` or `..` segments.

**Parameters:**

- `{path}` (`string`) Relative or absolute path. `~/` is expanded.

**Returns:** (`string`) Absolute path.

**Example:**

```lua
n00n.fs.abspath("src/main.rs") -- "/home/user/project/src/main.rs"
```

---

### `n00n.fs.resolve_within()` {#n00n-fs-resolve_within}

```lua
n00n.fs.resolve_within({base}, {candidate})
```

Resolve a candidate path physically and require it to remain below a base directory.
Existing symbolic links are followed before the boundary comparison. Non-existent tail
components are preserved below the last existing physical parent.

**Parameters:**

- `{base}` (`string`) Boundary directory.
- `{candidate}` (`string`) Candidate path.

**Returns:** (`string?`, `string?`) Resolved path, or nil and an error.

---

### `n00n.fs.read_within()` {#n00n-fs-read_within}

```lua
n00n.fs.read_within({base}, {relative})
```

Read a UTF-8 file relative to an opened base directory without following symbolic links.
Available only on Unix targets.

**Parameters:**

- `{base}` (`string`) Trusted base directory.
- `{relative}` (`string`) Relative file path.

**Returns:** (`string?`, `string?`) File contents, or nil plus an error.

---

### `n00n.fs.metadata_within()` {#n00n-fs-metadata_within}

```lua
n00n.fs.metadata_within({base}, {relative})
```

Get no-follow metadata for a path relative to an opened base directory.
Available only on Unix targets. The returned table contains `size`, `is_file`, `is_dir`,
`is_symlink`, and `mtime`; `mtime` is nanoseconds since the Unix epoch.

**Parameters:**

- `{base}` (`string`) Trusted base directory.
- `{relative}` (`string`) Relative path.

**Returns:** (`table?`, `string?`) Metadata, nil if missing, or nil plus an error.

---

### `n00n.fs.dir_within()` {#n00n-fs-dir_within}

```lua
n00n.fs.dir_within({base})
```

List one opened base directory without following symbolic links.
Available only on Unix targets.

**Parameters:**

- `{base}` (`string`) Trusted base directory.

**Returns:** (`table?`, `string?`) Directory entries, or nil plus an error.

---

### `n00n.fs.write_within()` {#n00n-fs-write_within}

```lua
n00n.fs.write_within({base}, {relative}, {content})
```

Atomically write a file relative to an opened base directory without following symbolic links.
Available only on Unix targets.

**Parameters:**

- `{base}` (`string`) Trusted base directory.
- `{relative}` (`string`) Relative destination path.
- `{content}` (`string`) Text to write.

**Returns:** (`true?`, `string?`) True on success, or nil plus an error.

---

### `n00n.fs.rm_within()` {#n00n-fs-rm_within}

```lua
n00n.fs.rm_within({base}, {relative})
```

Delete a file or symbolic link relative to an opened base directory without following links.
Available only on Unix targets.

**Parameters:**

- `{base}` (`string`) Trusted base directory.
- `{relative}` (`string`) Relative path.

**Returns:** (`true?`, `string?`) True on success, or nil plus an error.

---

### `n00n.fs.with_lock()` {#n00n-fs-with_lock}

```lua
n00n.fs.with_lock({path}, {callback})
```

Run a callback while holding an exclusive advisory lock on a file.
The lock is shared across independent Lua hosts and operating-system processes.
Available only on Unix targets.

**Parameters:**

- `{path}` (`string`) Lock file path. Its parent directory must exist.
- `{callback}` (`function`) Callback returning a `(value, err)` pair.

**Returns:** (`any?`, `string?`) Callback result, or nil and a lock error.

---

### `n00n.fs.parents()` {#n00n-fs-parents}

```lua
n00n.fs.parents({path})
```

Return all ancestor directories of {path}, from the immediate parent up to the root.
Handy for walking up a directory tree.

**Parameters:**

- `{path}` (`string`) File or directory path.

**Returns:** (`string[]`) Array of ancestor directory paths.

**Example:**

```lua
local dirs = n00n.fs.parents("/home/user/project/src")
-- { "/home/user/project", "/home/user", "/home", "/" }
```

---

### `n00n.fs.root()` {#n00n-fs-root}

```lua
n00n.fs.root({source}, {marker})
```

Walk upward from {source} looking for a directory that contains one of the
{marker} files or directories. Like `vim.fs.root`. Useful for finding the
project root.

**Parameters:**

- `{source}` (`string`) Starting file or directory path.
- `{marker}` (`string|string[]`) Marker filename(s) to look for, e.g. `".git"` or `{"package.json", ".git"}`.

**Returns:** (`string?`, `string?`) Root directory path, or nil when not found.

**Example:**

```lua
local root = n00n.fs.root("src/main.rs", { ".git", "Cargo.toml" })
if root then print("project root: " .. root) end
```

---

### `n00n.fs.relpath()` {#n00n-fs-relpath}

```lua
n00n.fs.relpath({base}, {target})
```

Compute a relative path from {base} to {target}.

**Parameters:**

- `{base}` (`string`) Base directory path.
- `{target}` (`string`) Target path.

**Returns:** (`string`) Relative path from {base} to {target}.

**Example:**

```lua
n00n.fs.relpath("/home/user", "/home/user/project/src") -- "project/src"
```

---

### `n00n.fs.ext()` {#n00n-fs-ext}

```lua
n00n.fs.ext({path})
```

Return the file extension of {path}, without the leading dot.

**Parameters:**

- `{path}` (`string`) File path.

**Returns:** (`string?`) Extension, or nil if the path has no extension.

**Example:**

```lua
n00n.fs.ext("main.rs")   -- "rs"
n00n.fs.ext("Makefile")  -- nil
```

---

### `n00n.fs.dir()` {#n00n-fs-dir}

```lua
n00n.fs.dir({path}, {opts?})
```

List the contents of the directory at {path}.
Each entry is a two-element array `{name, type}` where type is one of
`"file"`, `"directory"`, `"link"`, or `"unknown"`. Follows symlinks.

**Parameters:**

- `{path}` (`string`) Directory path.
- `{opts?}` (`table?`) `depth` (integer, default 1): how many levels deep to recurse.

**Returns:** (`table?`, `string?`) Array of `{name, type}` entries, or nil plus an error message.

**Example:**

```lua
local entries, err = n00n.fs.dir("src", { depth = 2 })
if err then return end
for _, e in ipairs(entries) do
  print(e[1], e[2]) -- "main.rs"  "file"
end
```

---

### `n00n.fs.write()` {#n00n-fs-write}

```lua
n00n.fs.write({path}, {content})
```

Write {content} to the file at {path}, creating it if it does not exist
or overwriting it if it does.

**Parameters:**

- `{path}` (`string`) Destination file path. `~/` is expanded.
- `{content}` (`string`) Text to write.

**Returns:** (`true?`, `string?`) `true` on success, or nil plus an error message.

**Example:**

```lua
local ok, err = n00n.fs.write("out.txt", "hello world")
if err then print("write failed: " .. err) end
```

---

### `n00n.fs.rm()` {#n00n-fs-rm}

```lua
n00n.fs.rm({path}, {opts?})
```

Delete the file, symlink, or directory at {path}.
Pass `recursive = true` to remove a non-empty directory tree (like `rm -r`).
Unlike `vim.fs.rm`, this also removes an empty directory without `recursive`.
Symlinks are removed themselves, never followed.

**Parameters:**

- `{path}` (`string`) Path to the file or directory to remove.
- `{opts?}` (`table?`) `recursive` (boolean, default false): remove a directory and its contents recursively. `force` (boolean, default false): silently ignore a missing path.

**Returns:** (`true?`, `string?`) `true` on success, or nil plus an error message.

**Example:**

```lua
local ok, err = n00n.fs.rm("temp.txt")
if err then print("rm failed: " .. err) end
n00n.fs.rm("stale_dir", { recursive = true, force = true })
```

---

### `n00n.fs.mkdir()` {#n00n-fs-mkdir}

```lua
n00n.fs.mkdir({path}, {opts?})
```

Create the directory at {path}. Set `parents = true` to create
intermediate directories, like `mkdir -p`.

**Parameters:**

- `{path}` (`string`) Directory path to create.
- `{opts?}` (`table?`) `parents` (boolean, default false): create intermediate parent directories.

**Returns:** (`true?`, `string?`) `true` on success, or nil plus an error message.

**Example:**

```lua
n00n.fs.mkdir("a/b/c", { parents = true })
```

---

### `n00n.fs.glob()` {#n00n-fs-glob}

```lua
n00n.fs.glob({pattern}, {opts?})
```

Find files matching one or more glob patterns.
Respects `.gitignore` by default. Pass `sort = "mtime"` to get the most
recently modified files first.

**Parameters:**

- `{pattern}` (`string|string[]`) Glob pattern or array of patterns.
- `{opts?}` (`table?`) `path` (string): search root. `limit` (integer): max results. `gitignore` (boolean, default true): respect .gitignore. `sort` (string): `"mtime"` sorts newest first.

**Returns:** (`string[]?`, `string?`) Array of absolute file paths, or nil plus an error message.

**Example:**

```lua
local files, err = n00n.fs.glob("**/*.lua", { path = "plugins", limit = 10 })
if err then return end
for _, f in ipairs(files) do print(f) end
```

---

### `n00n.fs.grep()` {#n00n-fs-grep}

```lua
n00n.fs.grep({pattern}, {opts?})
```

Search file contents for a regex {pattern}. Returns structured matches
grouped by file, similar to ripgrep output.

Each result entry has a `path` and a list of `groups`. Each group contains
`lines`, where every line has `line_nr`, `text`, and `is_match`.

**Parameters:**

- `{pattern}` (`string`) Regular expression to search for.
- `{opts?}` (`table?`) `path` (string): search root. `include` (string): file glob filter (e.g. `"*.rs"`). `context_before` / `context_after` (integer): context lines around matches. `limit` (integer): max match groups. `max_line_bytes` (integer): skip lines longer than this.

**Returns:** (`table?`, `string?`) Array of `{path, groups}` tables, or nil plus an error message.

**Example:**

```lua
local hits, err = n00n.fs.grep("TODO", { path = "src", include = "*.rs", limit = 5 })
if err then return end
for _, file in ipairs(hits) do
  for _, g in ipairs(file.groups) do
    for _, line in ipairs(g.lines) do
      if line.is_match then print(file.path .. ":" .. line.line_nr) end
    end
  end
end
```


## n00n.image {#n00n-image}

Small building blocks for working with images: probe metadata, decode
pixels, resize, and encode back to bytes. Plugins compose these freely.

Decoding is guarded against pixel-bomb attacks (50 MP limit).

```lua
local img = n00n.image.decode(raw_bytes)
local small = img:resize(1024, 768)
local png = small:encode("png")
```

---

### `n00n.image.probe()` {#n00n-image-probe}

```lua
n00n.image.probe({data})
```

Read image metadata (format, dimensions) from raw bytes without fully
decoding the pixels. Much faster than `decode` when you only need to
check the size or format.

Returns a table with `format` (string), `width` (integer), `height`
(integer), and `animated` (boolean; GIF/WebP streams with multiple frames),
or `(nil, err)` if the bytes are not a recognized image.

**Parameters:**

- `{data}` (`string|buffer`) Raw image bytes.

**Returns:** (`table?`, `string?`) Info table, or `(nil, err)` on failure.

**Example:**

```lua
local info, err = n00n.image.probe(raw_bytes)
if err then error(err) end
print(info.format, info.width, info.height)
```

---

### `n00n.image.decode()` {#n00n-image-decode}

```lua
n00n.image.decode({data})
```

Decode raw image bytes into an Image handle you can resize and re-encode.
Images larger than 50 megapixels are rejected to prevent memory bombs.

**Parameters:**

- `{data}` (`string|buffer`) Raw image bytes.

**Returns:** ([`n00n.image.Image?`](#n00n-image-Image), `string?`) Decoded image, or `(nil, err)` on failure.

**Example:**

```lua
local img, err = n00n.image.decode(raw_bytes)
if err then error(err) end
print(img:width() .. "x" .. img:height())
```


## n00n.image.Image {#n00n-image-Image}

A decoded image you can inspect, resize, crop, and re-encode.

Get one from `n00n.image.decode()`. The image data lives in memory
until the handle is garbage collected.

---

### `Image:width()` {#Image-width}

```lua
Image:width()
```

Get the width of the image in pixels.

**Returns:** (`integer`) Width in pixels.

---

### `Image:height()` {#Image-height}

```lua
Image:height()
```

Get the height of the image in pixels.

**Returns:** (`integer`) Height in pixels.

---

### `Image:resize()` {#Image-resize}

```lua
Image:resize({max_w}, {max_h})
```

Shrink the image to fit inside {max_w} x {max_h}, keeping the aspect
ratio. If the image already fits, it is returned as-is. Never upscales.

**Parameters:**

- `{max_w}` (`integer`) Maximum width in pixels. Must be positive.
- `{max_h}` (`integer`) Maximum height in pixels. Must be positive.

**Returns:** ([`n00n.image.Image`](#n00n-image-Image)) A new image handle (or the same one if no resize was needed).

**Example:**

```lua
local img = n00n.image.decode(raw_bytes)
local small = img:resize(800, 600)
local encoded = small:encode("jpeg")
```

---

### `Image:crop()` {#Image-crop}

```lua
Image:crop({x}, {y}, {width}, {height})
```

Copy a rectangular pixel region without resizing it. Coordinates are
zero-based source pixels. The original image is unchanged.

**Parameters:**

- `{x}` (`integer`) Left edge in source pixels.
- `{y}` (`integer`) Top edge in source pixels.
- `{width}` (`integer`) Crop width in pixels. Must be positive.
- `{height}` (`integer`) Crop height in pixels. Must be positive.

**Returns:** ([`n00n.image.Image`](#n00n-image-Image)) A new image handle containing the crop.

**Example:**

```lua
local tile = img:crop(0, 2000, 1440, 2000)
local png = tile:encode("png")
```

---

### `Image:encode()` {#Image-encode}

```lua
Image:encode({format})
```

Encode the image into raw bytes in the given format. Use this to prepare
images for sending over the network or writing to disk.

**Parameters:**

- `{format}` (`string`) Output format: `"png"`, `"jpeg"`, or `"jpg"`.

**Returns:** (`string`) Encoded image bytes.

**Example:**

```lua
local bytes = img:encode("png")
-- bytes is a Lua string containing the raw PNG data
```

---

### `Image:encode_limited()` {#Image-encode_limited}

```lua
Image:encode_limited({format}, {max_bytes})
```

Encode with a strict output-byte limit. Use this for untrusted or
transport-bound image output so encoding cannot grow a `Vec` without bound.

**Parameters:**

- `{format}` (`string`) Output format: `"png"`, `"jpeg"`, or `"jpg"`.
- `{max_bytes}` (`integer`) Maximum encoded bytes, up to 3,750,000.

**Returns:** (`string?`, `string?`) Encoded bytes, or nil plus an error when the limit is exceeded.


## n00n.interpreter {#n00n-interpreter}

Run Python code in a memory-safe, time-limited sandbox.

The sandbox uses the monty interpreter. Python code can call back into
Lua-defined tools, and stdout is streamed line by line. Requires the
`run` permission.

```lua
local r, err = n00n.interpreter.run("print('hello')", {
  timeout = 10,
  max_memory_mb = 128,
  on_output = function(line) print(line) end,
})
```

---

### `n00n.interpreter.run()` {#n00n-interpreter-run}

```lua
n00n.interpreter.run({code}, {opts})
```

Run Python code in a sandboxed interpreter with memory and time limits.
Stdout lines are streamed to your {on_output} callback as they are produced.
If the Python code calls tools, those calls are dispatched to the Lua
functions you provide in {opts}.tools.

The result table has optional fields: `stdout` (string, trimmed combined
output) and `output` (string, the final expression value). On error, the
table is empty and the second return value is the error message.

**Parameters:**

- `{code}` (`string`) Python source code to execute.
- `{opts}` (`table`) Required fields:
  - `timeout` (`integer`) execution time limit in seconds.
  - `max_memory_mb` (`integer`) memory limit in megabytes.
  - `on_output` (`function`) called with each stdout line (string) as it is
    produced. Must not yield.

  Optional fields:

  - `ruff_fix` (`boolean?`) run Ruff fix/unsafe-fixes and formatting before execution.
  - `tools` (`table?`) map of `name -> function` for tools the sandbox may call.
    Each function receives the tool input table and must return `(string)` or
    `(nil, err)`. Tool calls are batched and dispatched concurrently.

**Returns:** (`table`, `string?`) Result table, plus an error string on failure.

**Example:**

```lua
local result, err = n00n.interpreter.run("print(2 + 2)", {
  timeout = 30,
  max_memory_mb = 256,
  on_output = function(line) print("py: " .. line) end,
})
if err then error(err) end
if result.stdout then print(result.stdout) end
```


## n00n.json {#n00n-json}

JSON encoding, decoding, schema validation, and TOON round-trip.
Encode Lua tables to JSON strings, decode JSON back into tables,
validate against a JSON Schema, or convert to/from TOON for
token-efficient context blocks.

```lua
local s = n00n.json.encode({ ok = true })
local t = n00n.json.decode(s)
```

---

### `n00n.json.encode()` {#n00n-json-encode}

```lua
n00n.json.encode({value})
```

Turn a Lua value into a JSON string. Tables, strings, numbers,
booleans, and nil all work. Functions and userdata cannot be
serialized.

**Parameters:**

- `{value}` (`any`) Lua value to encode.

**Returns:** (`string?`, `string?`) JSON string, or nil plus an error.

**Example:**

```lua
local s, err = n00n.json.encode({ name = "n00n", version = 1 })
print(s) -- {"name":"n00n","version":1}
```

---

### `n00n.json.decode()` {#n00n-json-decode}

```lua
n00n.json.decode({str})
```

Parse a JSON string into a Lua value. Objects become tables and
arrays become 1-indexed sequences.

**Parameters:**

- `{str}` (`string`) JSON string to decode.

**Returns:** (`any?`, `string?`) Decoded value, or nil plus an error.

**Example:**

```lua
local t, err = n00n.json.decode('{"x": 42}')
print(t.x) -- 42
```

---

### `n00n.json.schema_validator()` {#n00n-json-schema_validator}

```lua
n00n.json.schema_validator({schema})
```

Compile a JSON Schema into a reusable validator object. Supports
draft-07, 2019-09, and 2020-12. Schema errors show up right away so
you catch mistakes before doing any real work.

**Parameters:**

- `{schema}` (`table`) JSON Schema as a Lua table.

**Returns:** ([`n00n.json.SchemaValidator?`](#n00n-json-SchemaValidator), `string?`) Validator, or nil plus an error.

**Example:**

```lua
local v, err = n00n.json.schema_validator({
  type = "object",
  properties = { name = { type = "string" } },
  required = { "name" },
})
local errs = v:validate({ name = "n00n" })
assert(errs == nil)
```

---

### `n00n.json.to_toon()` {#n00n-json-to_toon}

```lua
n00n.json.to_toon({value})
```

Encode a Lua value as TOON (Token-Oriented Object Notation), a token-efficient
alternative to JSON for LLM context (~30-60% fewer tokens on uniform arrays of
objects). Opt-in: pair with `from_toon` only when the consumer is a model.

**Parameters:**

- `{value}` (`any`) Lua value to encode.

**Returns:** (`string?`, `string?`) TOON string, or nil plus an error.

**Example:**

```lua
local s, err = n00n.json.to_toon({ users = { { id = 1, name = "Alice" } } })
```

---

### `n00n.json.from_toon()` {#n00n-json-from_toon}

```lua
n00n.json.from_toon({str})
```

Decode a TOON string back into a Lua value. Inverse of `to_toon`.

**Parameters:**

- `{str}` (`string`) TOON string to decode.

**Returns:** (`any?`, `string?`) Decoded value, or nil plus an error.

**Example:**

```lua
local t, err = n00n.json.from_toon(s)
```

---

### `n00n.json.tooned()` {#n00n-json-tooned}

```lua
n00n.json.tooned({value})
```

Lossless JSON/TOON passthrough. Encodes the value as JSON and TOON and
returns whichever representation is smaller. If TOON does not shrink the
payload, the original JSON string is returned unchanged.

**Parameters:**

- `{value}` (`any`) Lua value to encode.

**Returns:** (`string?`, `string?`) Encoded string (JSON or TOON) and its format ("json" or "toon"), or nil plus an error.

**Example:**

```lua
local s, fmt = n00n.json.tooned({ users = { { id = 1, name = "Alice" } } })
```

---

### `n00n.json.toon_stats()` {#n00n-json-toon_stats}

```lua
n00n.json.toon_stats()
```

Return historical TOON passthrough statistics.

**Returns:** (`table?`, `string?`) Stats table with calls, json_bytes, toon_bytes, toon_wins, saved_bytes, or nil plus an error.

**Example:**

```lua
local stats, err = n00n.json.toon_stats()
```


## n00n.json.SchemaValidator {#n00n-json-SchemaValidator}

A compiled JSON Schema validator. Create one with `n00n.json.schema_validator()` and reuse it to validate many values without recompiling the schema each time.

---

### `SchemaValidator:validate()` {#SchemaValidator-validate}

```lua
SchemaValidator:validate({value})
```

Check {value} against the compiled schema. Returns nil when the value is valid. When validation fails, returns a list of human-readable error strings.

**Parameters:**

- `{value}` (`any`) The Lua value to validate.

**Returns:** (`table?`) Array of error strings, or nil if valid.

**Example:**

```lua
local errs = validator:validate({ name = 123 })
if errs then
for _, msg in ipairs(errs) do print(msg) end
end
```


## n00n.keymap {#n00n-keymap}

Key mappings, modeled after `vim.keymap`. If you have written a
Neovim keymap plugin before, this will feel familiar.

```lua
n00n.keymap.set("n", "<C-t>", function()
  print("hello")
end, { desc = "Say hello" })
```

---

### `n00n.keymap.set()` {#n00n-keymap-set}

```lua
n00n.keymap.set({mode}, {lhs}, {rhs}, {opts?})
```

Bind a key to a Lua function, just like `vim.keymap.set`. Only
normal mode (`"n"`) is supported right now. If {lhs} is already
mapped, the old binding is replaced and a warning is logged.

**Parameters:**

- `{mode}` (`string`) Mode letter. Currently only `"n"` is accepted.
- `{lhs}` (`string`) Key in Vim notation, e.g. `"<C-t>"`, `"<Space>"`, `"a"`.
- `{rhs}` (`function`) Called when the key is pressed.
- `{opts?}` (`table?`) Options:
  - `desc` (`string`) short description shown in the keymap list.

**Example:**

```lua
n00n.keymap.set("n", "<C-t>", function()
  print("toggle!")
end, { desc = "Toggle panel" })
```

---

### `n00n.keymap.del()` {#n00n-keymap-del}

```lua
n00n.keymap.del({mode}, {lhs})
```

Remove the mapping for {lhs} in {mode}. Does nothing if no mapping
exists for that key.

**Parameters:**

- `{mode}` (`string`) Mode letter (reserved for future modes).
- `{lhs}` (`string`) Key to unmap, in Vim notation.

**Example:**

```lua
n00n.keymap.del("n", "<C-t>")
```


## n00n.log {#n00n-log}

Structured logging for plugins.

Each call emits a tracing event tagged with the calling plugin's name.
Messages show up in n00n's log output, which you can view with `n00n --log`.

```lua
n00n.log.info("ready")
n00n.log.warn("something looks off")
```

---

### `n00n.log.debug()` {#n00n-log-debug}

```lua
n00n.log.debug({msg})
```

Emit a DEBUG-level log message. Useful for development and troubleshooting.
The message is tagged with the plugin name automatically.

**Parameters:**

- `{msg}` (`string`) Message to log.

**Example:**

```lua
n00n.log.debug("loaded " .. #items .. " items")
```

---

### `n00n.log.info()` {#n00n-log-info}

```lua
n00n.log.info({msg})
```

Emit an INFO-level log message. Good for normal operational events.

**Parameters:**

- `{msg}` (`string`) Message to log.

**Example:**

```lua
n00n.log.info("plugin initialized")
```

---

### `n00n.log.warn()` {#n00n-log-warn}

```lua
n00n.log.warn({msg})
```

Emit a WARN-level log message. Use for recoverable problems.

**Parameters:**

- `{msg}` (`string`) Message to log.

**Example:**

```lua
n00n.log.warn("config file missing, using defaults")
```

---

### `n00n.log.error()` {#n00n-log-error}

```lua
n00n.log.error({msg})
```

Emit an ERROR-level log message. Use for failures that need attention.

**Parameters:**

- `{msg}` (`string`) Message to log.

**Example:**

```lua
n00n.log.error("failed to connect to API")
```


## n00n.net {#n00n-net}

HTTP client for fetching web content. All traffic goes over HTTPS
(plain HTTP is upgraded). Private and metadata IP addresses are
blocked to prevent SSRF. Failed requests (5xx) are retried
automatically.

```lua
local res, err = n00n.net.request("https://example.com")
if res then print(res.body) end
```

---

### `n00n.net.request()` {#n00n-net-request}

```lua
n00n.net.request({url}, {opts?})
```

Make an HTTP request and return the response body. Plain `http://`
URLs are automatically upgraded to `https://`. Requests to private
or metadata IP addresses are blocked for safety.

{opts} fields:
  `method` (string) HTTP verb (default `"GET"`).
  `headers` (table) Header name/value pairs.
  `body` (string) Request body.
  `timeout` (integer) Timeout in seconds, max 120 (default 30).
  `max_bytes` (integer) Max response size in bytes (default 5 MB).
  `retry` (integer) Retries on 5xx errors (default 3).

The response table has three fields: `body` (string), `status`
(integer), and `content_type` (string).

**Parameters:**

- `{url}` (`string`) URL starting with `http://` or `https://`.
- `{opts?}` (`table?`) Request options (see above).

**Returns:** (`table?`, `string?`) Response table, or nil plus an error string.

**Example:**

```lua
local res, err = n00n.net.request("https://httpbin.org/get")
if err then
  print("failed: " .. err)
else
  print(res.status, res.body)
end
```


## n00n.search {#n00n-search}

Native, keyless extraction of bounded public web content.

---

### `n00n.search.extract()` {#n00n-search-extract}

```lua
n00n.search.extract(ctx, request)
```

Extract public URLs with manual redirect validation, DNS pinning, byte limits, and tool cancellation. Requires [search].enabled = true and the plugin's net permission.

**Parameters:**

- `ctx` (`LuaCtx`) Current tool context; cancellation stops the operation.
- `request` (`table`) Extraction request. Fields: urls (array of 1 to 20 public http(s) URLs), format ("markdown", "text", or "html"), max_bytes_per_source (1 to 10485760 bytes).

**Returns:** (`table|nil`, `string|nil`) Extraction response or an error.

**Example:**

```lua
local result, err = n00n.search.extract(ctx, {
  urls = { "https://example.com/doc" },
  format = "markdown",
  max_bytes_per_source = 262144,
})
```


## n00n.session {#n00n-session}

Host session primitives. The interactive UI can run several sessions
at once; these functions let plugins list, create, focus, rename, and
delete them. Every call round-trips to the UI event loop and returns
the pair `(value, err)`. Without an interactive UI attached, every
call returns `nil, "no interactive UI attached"`.

---

### `n00n.session.list()` {#n00n-session-list}

```lua
n00n.session.list()
```

Lists sessions stored for the current project. Answered from a
background scan, so a slow disk never blocks the UI.

**Returns:** (`table|nil`, `string|nil`) Array of `{id, title, display_title, kind,
parent_id, updated_at, cwd, model}`, or nil and an error.

**Example:**

```lua
local stored, err = n00n.session.list()
```

---

### `n00n.session.live()` {#n00n-session-live}

```lua
n00n.session.live()
```

Lists the sessions currently running in this UI. Status is "working",
"needs_input", or "idle".

**Returns:** (`table|nil`, `string|nil`) Array of `{id, title, status, updated_at, focused}`, or nil and an error.

**Example:**

```lua
local live, err = n00n.session.live()
```

---

### `n00n.session.status()` {#n00n-session-status}

```lua
n00n.session.status({id})
```

Returns one live session with its status, latest assistant text output,
and paused team run metadata when the latest tool result is from `team`.

**Parameters:**

- `{id}` (`string`) Live session id.

**Returns:** (`table|nil`, `string|nil`) `{id, title, status, updated_at, focused, output?, paused_team?}` where `paused_team` is `{paused, run_id, mode?, ...}` when a paused team run is present, or nil and an error.

---

### `n00n.session.current()` {#n00n-session-current}

```lua
n00n.session.current()
```

Returns the id of the currently focused session.

**Returns:** (`string|nil`, `string|nil`) Session id, or nil and an error.

**Example:**

```lua
local id = n00n.session.current()
```

---

### `n00n.session.focus()` {#n00n-session-focus}

```lua
n00n.session.focus({id})
```

Switches the UI to the session with {id}. The session must be live.

**Parameters:**

- `{id}` (`string`) Session id, as returned by `list()` or `live()`.

**Returns:** (`boolean|nil`, `string|nil`) true on success, or nil and an error.

**Example:**

```lua
local _, err = n00n.session.focus(id)
```

---

### `n00n.session.delete()` {#n00n-session-delete}

```lua
n00n.session.delete({id})
```

Deletes a session and its stored history, cancelling it first if it
is running. The focused session cannot be deleted.

**Parameters:**

- `{id}` (`string`) Session id to delete.

**Returns:** (`boolean|nil`, `string|nil`) true on success, or nil and an error.

**Example:**

```lua
local _, err = n00n.session.delete(id)
```

---

### `n00n.session.new()` {#n00n-session-new}

```lua
n00n.session.new({opts?})
```

Starts a new session in the current project.

**Parameters:**

- `{opts?}` (`table?`) Optional fields: prompt (string) first user message

  to submit right away; focus (boolean) switch the UI to the new session;

  - `parent_id` (`string?`) session that spawned this session; tool (string) for a

  direct host-executed bootstrap. Tool cannot be combined with prompt. Input


  (table) and title (string?) require tool.


**Returns:** (`string|nil`, `string|nil`) New session id, or nil and an error.

**Example:**

```lua
local id, err = n00n.session.new({ prompt = "fix the tests", focus = true })
```

---

### `n00n.session.prompt()` {#n00n-session-prompt}

```lua
n00n.session.prompt({text}, {opts?})
```

Sends {text} as a regular user prompt to a live session. The text is
never interpreted: slash commands, `exit`, and `!` shell prefixes are
all sent to the model verbatim. If the session is currently streaming,
the prompt is queued and picked up when the agent reaches it.

**Parameters:**

- `{text}` (`string`) The prompt to send. Must not be blank.
- `{opts?}` (`table?`) Optional fields: session (string) id of a live

  session (defaults to the focused one); steer (boolean) request


  delivery as a steering interrupt when the session is busy; control


  (boolean) mark the message as an agent-to-agent control message.


**Returns:** (`string|nil`, `string|nil`) "started" or "queued", or nil and an error.

**Example:**

```lua
local state, err = n00n.session.prompt("run the tests", { session = id, steer = true, control = true })
```

---

### `n00n.session.cancel()` {#n00n-session-cancel}

```lua
n00n.session.cancel({id})
```

Cancels the current turn in a live session without deleting the session.

**Parameters:**

- `{id}` (`string`) Live session id.

**Returns:** (`boolean|nil`, `string|nil`) true on success, or nil and an error.

---

### `n00n.session.set_title()` {#n00n-session-set_title}

```lua
n00n.session.set_title({opts})
```

Renames a session, live or stored.

**Parameters:**

- `{opts}` (`table`) Required fields: id (string) session to rename;
  - `title` (`string`) the new title.

**Returns:** (`boolean|nil`, `string|nil`) true on success, or nil and an error.

**Example:**

```lua
local _, err = n00n.session.set_title({ id = id, title = "refactor" })
```


## n00n.text {#n00n-text}

Text transformation utilities.

Helper functions for converting between text formats.

```lua
local md = n00n.text.html_to_markdown(html)
```

---

### `n00n.text.html_to_markdown()` {#n00n-text-html_to_markdown}

```lua
n00n.text.html_to_markdown({html})
```

Convert an HTML string to Markdown.
Useful for cleaning up web content fetched with `n00n.webfetch`.

**Parameters:**

- `{html}` (`string`) HTML source text.

**Returns:** (`string?`, `string?`) Markdown text on success, or nil plus an error message.

**Example:**

```lua
local md, err = n00n.text.html_to_markdown("<h1>Hello</h1><p>world</p>")
if err then return end
print(md) -- "# Hello\n\nworld"
```


## n00n.treesitter {#n00n-treesitter}

Tree-sitter parsing and query API.

Mirrors `vim.treesitter` from Neovim, so plugins can be shared between the two.
Start with `get_parser()` to parse source code, then use `get_node_text()` and
the `query` sub-module to extract information from the syntax tree.

```lua
local parser, err = n00n.treesitter.get_parser(source, "lua")
local trees = parser:parse()
local root = trees[1]:root()
```

---

### `n00n.treesitter.get_parser()` {#n00n-treesitter-get_parser}

```lua
n00n.treesitter.get_parser({source}, {lang})
```

Creates a `LanguageTree` for {source} using the grammar named {lang}.
This is the main entry point for parsing source code with tree-sitter.
Signature matches `vim.treesitter.get_parser()`, so Neovim plugins can be copy-pasted.

**Parameters:**

- `{source}` (`string`) Source text to parse.
- `{lang}` (`string`) Language name, e.g. `"rust"` or `"lua"`.

**Returns:** ([`LanguageTree|nil`](#n00n-treesitter-LanguageTree), `string|nil`) Parser, or nil and an error message.

**Example:**

```lua
local parser, err = n00n.treesitter.get_parser(src, "lua")
if err then print("error: " .. err) end
```

---

### `n00n.treesitter.get_string_parser()` {#n00n-treesitter-get_string_parser}

```lua
n00n.treesitter.get_string_parser({source}, {lang})
```

Alias for `get_parser`. Use whichever name you prefer.

**Parameters:**

- `{source}` (`string`) Source text to parse.
- `{lang}` (`string`) Language name.

**Returns:** ([`LanguageTree|nil`](#n00n-treesitter-LanguageTree), `string|nil`) Parser, or nil and an error message.

---

### `n00n.treesitter.get_node_text()` {#n00n-treesitter-get_node_text}

```lua
n00n.treesitter.get_node_text({node}, {source})
```

Gets the text that {node} covers in {source}.
Useful when you have a captured node and need the actual source substring.

**Parameters:**

- `{node}` ([`Node`](#n00n-treesitter-Node)) The node whose text you want.
- `{source}` (`string`) Original source text the tree was parsed from.

**Returns:** (`string`) Substring covered by the node.

**Example:**

```lua
local text = n00n.treesitter.get_node_text(node, source)
print(text)
```

---

### `n00n.treesitter.get_node_range()` {#n00n-treesitter-get_node_range}

```lua
n00n.treesitter.get_node_range({node})
```

Returns the range of {node} as four 0-based integers: start_row, start_col, end_row, end_col.

**Parameters:**

- `{node}` ([`Node`](#n00n-treesitter-Node)) The node to query.

**Returns:** (`integer`, `integer`, `integer`, `integer`) start_row, start_col, end_row, end_col.

**Example:**

```lua
local sr, sc, er, ec = n00n.treesitter.get_node_range(node)
```

---

### `n00n.treesitter.get_range()` {#n00n-treesitter-get_range}

```lua
n00n.treesitter.get_range({node})
```

Returns a six-element table for {node}: `{start_row, start_col, start_byte, end_row, end_col, end_byte}`.
This gives you byte offsets in addition to row/column positions.

**Parameters:**

- `{node}` ([`Node`](#n00n-treesitter-Node)) The node to query.

**Returns:** (`table`) Six-element array: start_row, start_col, start_byte, end_row, end_col, end_byte.

**Example:**

```lua
local r = n00n.treesitter.get_range(node)
print("bytes: " .. r[3] .. "-" .. r[6])
```

---

### `n00n.treesitter.is_ancestor()` {#n00n-treesitter-is_ancestor}

```lua
n00n.treesitter.is_ancestor({dest}, {source})
```

Checks whether {dest} is an ancestor of {source} (or the same node).
Walks up from {source} toward the root looking for {dest}.

**Parameters:**

- `{dest}` ([`Node`](#n00n-treesitter-Node)) Potential ancestor node.
- `{source}` ([`Node`](#n00n-treesitter-Node)) Node to check ancestry for.

**Returns:** (`boolean`)

---

### `n00n.treesitter.is_in_node_range()` {#n00n-treesitter-is_in_node_range}

```lua
n00n.treesitter.is_in_node_range({node}, {line}, {col})
```

Checks whether the 0-based position ({line}, {col}) falls inside {node}.
Handy for cursor-position checks.

**Parameters:**

- `{node}` ([`Node`](#n00n-treesitter-Node)) Node to test against.
- `{line}` (`integer`) 0-based line number.
- `{col}` (`integer`) 0-based column number.

**Returns:** (`boolean`)

---

### `n00n.treesitter.node_contains()` {#n00n-treesitter-node_contains}

```lua
n00n.treesitter.node_contains({node}, {range})
```

Checks whether {node} fully contains the given {range}.

**Parameters:**

- `{node}` ([`Node`](#n00n-treesitter-Node)) Node to test.
- `{range}` (`table`) Four-element array `{start_row, start_col, end_row, end_col}`.

**Returns:** (`boolean`)

---

### `n00n.treesitter.get_node()` {#n00n-treesitter-get_node}

```lua
n00n.treesitter.get_node({opts?})
```

Gets the node at the given cursor position in the source code.
Parses the source and returns the smallest node containing the position.

Mirrors `vim.treesitter.get_node()`. {opts} accepts `source`/`bufnr`, `lang`, `pos`, and `named`.

**Parameters:**

- `{opts?}` (`table`) Options for `get_node`: source (string, optional), bufnr (integer, optional), lang (string, required), pos ({row, col}, required), named (boolean, default true).

**Returns:** ([`Node|nil`](#n00n-treesitter-Node), `string?`) Node at position, or nil and an error message.

**Example:**

```lua
local node, err = n00n.treesitter.get_node({
  source = "local x = 1",
  lang = "lua",
  pos = {0, 6}
})
```


## n00n.treesitter.language {#n00n-treesitter-language}

Language registry for tree-sitter grammars.

Mirrors `vim.treesitter.language`. Use these functions to register grammars,
map filetypes to languages, and inspect available node types.

```lua
n00n.treesitter.language.add("lua")
n00n.treesitter.language.register("lua", "luau")
```

---

### `n00n.treesitter.language.add()` {#n00n-treesitter-language-add}

```lua
n00n.treesitter.language.add({lang}, {opts?})
```

Registers {lang} for use with tree-sitter.
Call this to confirm a language grammar is available.
Custom grammar paths are not yet supported.

**Parameters:**

- `{lang}` (`string`) Language name, e.g. `"rust"`.
- `{opts?}` (`table?`) Options table (the `path` key is not yet supported).

**Returns:** (`boolean`, `string?`) true on success, or false and an error message.

**Example:**

```lua
local ok, err = n00n.treesitter.language.add("lua")
if not ok then print("error: " .. err) end
```

---

### `n00n.treesitter.language.register()` {#n00n-treesitter-language-register}

```lua
n00n.treesitter.language.register({lang}, {filetype})
```

Associates {lang} with one or more filetypes, so you can look up the right
parser language for a given filetype later with `get_lang()`.

**Parameters:**

- `{lang}` (`string`) Language name.
- `{filetype}` (`string|table`) A single filetype string or an array of filetype strings.

**Example:**

```lua
n00n.treesitter.language.register("typescript", { "ts", "tsx" })
```

---

### `n00n.treesitter.language.get_lang()` {#n00n-treesitter-language-get_lang}

```lua
n00n.treesitter.language.get_lang({filetype})
```

Looks up the tree-sitter language name for {filetype}.
Returns the registered language, or falls back to {filetype} itself if
a grammar with that name exists. Returns nil when nothing matches.

**Parameters:**

- `{filetype}` (`string`) Filetype to look up, e.g. `"ts"`.

**Returns:** (`string|nil`) Language name, or nil.

**Example:**

```lua
local lang = n00n.treesitter.language.get_lang("tsx")
if lang then print(lang) end -- "typescript"
```

---

### `n00n.treesitter.language.get_filetypes()` {#n00n-treesitter-language-get_filetypes}

```lua
n00n.treesitter.language.get_filetypes({lang})
```

Returns all filetypes that have been registered for {lang}.

**Parameters:**

- `{lang}` (`string`) Language name.

**Returns:** (`table`) Array of filetype strings.

**Example:**

```lua
local fts = n00n.treesitter.language.get_filetypes("typescript")
-- { "ts", "tsx" }
```

---

### `n00n.treesitter.language.inspect()` {#n00n-treesitter-language-inspect}

```lua
n00n.treesitter.language.inspect({lang})
```

Returns metadata about the grammar for {lang}.
Useful for debugging or discovering which node types and fields a grammar defines.

**Parameters:**

- `{lang}` (`string`) Language name.

**Returns:** (`table`) Table with keys `abi_version` (integer), `node_types` (string[]), `fields` (string[]).

**Example:**

```lua
local info = n00n.treesitter.language.inspect("lua")
print("ABI: " .. info.abi_version)
for _, nt in ipairs(info.node_types) do print(nt) end
```


## n00n.treesitter.query {#n00n-treesitter-query}

Query compilation and lookup.

Mirrors `vim.treesitter.query`. Use `parse()` to compile a tree-sitter
query string into a `Query` object you can run against parsed trees.

```lua
local q = n00n.treesitter.query.parse("lua", "(string) @str")
```

---

### `n00n.treesitter.query.parse()` {#n00n-treesitter-query-parse}

```lua
n00n.treesitter.query.parse({lang}, {query})
```

Compiles a tree-sitter query string for {lang}.
Throws if the language is unknown or the query has a syntax error.

**Parameters:**

- `{lang}` (`string`) Language name, e.g. `"lua"`.
- `{query}` (`string`) Tree-sitter S-expression query.

**Returns:** ([`Query`](#n00n-treesitter-Query)) Compiled query object.

**Example:**

```lua
local q = n00n.treesitter.query.parse("lua", "(identifier) @id")
```

---

### `n00n.treesitter.query.get()` {#n00n-treesitter-query-get}

```lua
n00n.treesitter.query.get({lang}, {name})
```

Looks up a named built-in query for {lang}.
Returns a compiled query object from bundled query files.

**Parameters:**

- `{lang}` (`string`) Language name.
- `{name}` (`string`) Query name, e.g. `"highlights"`.

**Returns:** ([`Query|nil`](#n00n-treesitter-Query), `string?`) Query object, or nil and an error message.

**Example:**

```lua
local q, err = n00n.treesitter.query.get("lua", "highlights")
if q then print(#q.captures) end
```


## n00n.treesitter.Query {#n00n-treesitter-Query}

A compiled tree-sitter query.

Get one by calling `n00n.treesitter.query.parse(lang, query_string)`.
Then use `:iter_captures()` or `:iter_matches()` to run it against a syntax tree.

```lua
local q = n00n.treesitter.query.parse("lua", "(identifier) @id")
for idx, node, meta in q:iter_captures(root, source) do
  print(node:type())
end
```

---

### `Query:iter_captures()` {#Query-iter_captures}

```lua
Query:iter_captures({node}, {source}, {start_row?}, {stop_row?})
```

Iterates over every capture matched by this query. Each call to the returned iterator yields `(capture_index, node, metadata, match, active)`. Use this when you care about individual captures rather than whole pattern matches.

**Parameters:**

- `{node}` ([`Node`](#n00n-treesitter-Node)) Root node to search within.
- `{source}` (`string`) Source text the tree was parsed from.
- `{start_row?}` (`integer`) Only match rows >= this value (0-based).
- `{stop_row?}` (`integer`) Only match rows < this value (0-based).

**Returns:** (`function`) Iterator yielding (integer, Node, table, table, integer).

**Example:**

```lua
local q = n00n.treesitter.query.parse("lua", "(identifier) @id")
for idx, node, meta in q:iter_captures(root, source) do
  print(idx, node:type())
end
```

---

### `Query:iter_matches()` {#Query-iter_matches}

```lua
Query:iter_matches({node}, {source}, {start_row?}, {stop_row?})
```

Iterates over every full pattern match in this query. Each call to the returned iterator yields `(pattern_index, captures, metadata, active)` where captures is a table keyed by capture index. Use this when you need all captures for a pattern together.

**Parameters:**

- `{node}` ([`Node`](#n00n-treesitter-Node)) Root node to search within.
- `{source}` (`string`) Source text the tree was parsed from.
- `{start_row?}` (`integer`) Only match rows >= this value (0-based).
- `{stop_row?}` (`integer`) Only match rows < this value (0-based).

**Returns:** (`function`) Iterator yielding (integer, table, table, integer).

**Example:**

```lua
local q = n00n.treesitter.query.parse("lua", "(function_declaration name: (identifier) @name)"
)
for pat, captures, meta in q:iter_matches(root, source) do
  for cap_idx, nodes in pairs(captures) do
    print(nodes[1]:type())
  end
end
```


## n00n.treesitter.Tree {#n00n-treesitter-Tree}

A parsed syntax tree.

Obtained from `LanguageTree:parse()` or `LanguageTree:trees()`.
Call `:root()` to get the root node and start traversing.

```lua
local trees = parser:parse()
local root = trees[1]:root()
```

---

### `Tree:root()` {#Tree-root}

```lua
Tree:root()
```

Returns the root node of this tree. This is where you start walking
the syntax tree or running queries.

**Returns:** ([`Node`](#n00n-treesitter-Node)) Root node.

**Example:**

```lua
local root = tree:root()
print(root:type()) -- e.g. "chunk" for Lua
```

---

### `Tree:copy()` {#Tree-copy}

```lua
Tree:copy()
```

Returns an independent copy of this tree.
Edits to the copy will not affect the original.

**Returns:** ([`Tree`](#n00n-treesitter-Tree)) A new Tree with the same content.


## n00n.treesitter.Node {#n00n-treesitter-Node}

A single node in a parsed syntax tree.

Nodes are obtained from `Tree:root()`, navigation methods like `:child()`,
or from query captures. Each node knows its type, range, and children.

```lua
local root = tree:root()
print(root:type(), root:child_count())
for child, field in root:iter_children() do
  print(child:type(), field)
end
```

---

### `Node:type()` {#Node-type}

```lua
Node:type()
```

Returns the grammar type name for this node, like `"function_definition"` or `"identifier"`.

**Returns:** (`string`) Grammar type name.

---

### `Node:symbol()` {#Node-symbol}

```lua
Node:symbol()
```

Returns the numeric symbol id for this node's grammar type.
Two nodes with the same type always share the same symbol id.

**Returns:** (`integer`) Symbol id.

---

### `Node:id()` {#Node-id}

```lua
Node:id()
```

Returns a unique string identifier for this specific node in the tree.
Useful for deduplication or as a table key.

**Returns:** (`string`) Node identity string.

---

### `Node:range()` {#Node-range}

```lua
Node:range({include_bytes?})
```

Returns the range of this node as multiple return values.
Without {include_bytes}: `start_row, start_col, end_row, end_col`.
With {include_bytes} set to true: `start_row, start_col, start_byte, end_row, end_col, end_byte`.

**Parameters:**

- `{include_bytes?}` (`boolean`) When true, byte offsets are included in the return values.

**Returns:** (`integer`, `integer`, `integer`, `integer`) Four values, or six when include_bytes is true.

**Example:**

```lua
local sr, sc, er, ec = node:range()
local sr, sc, sb, er, ec, eb = node:range(true)
```

---

### `Node:start()` {#Node-start}

```lua
Node:start()
```

Returns the start position of this node: row, column, and byte offset (all 0-based).

**Returns:** (`integer`, `integer`, `integer`) start_row, start_col, start_byte.

---

### `Node:end_()` {#Node-end_}

```lua
Node:end_()
```

Returns the end position of this node: row, column, and byte offset (all 0-based).

**Returns:** (`integer`, `integer`, `integer`) end_row, end_col, end_byte.

---

### `Node:byte_length()` {#Node-byte_length}

```lua
Node:byte_length()
```

Returns how many bytes this node spans in the source text.

**Returns:** (`integer`) Byte length.

---

### `Node:child()` {#Node-child}

```lua
Node:child({index})
```

Returns the child at position {index} (0-based), including anonymous nodes like punctuation.
Returns nil if {index} is out of bounds.

**Parameters:**

- `{index}` (`integer`) 0-based child index.

**Returns:** ([`Node|nil`](#n00n-treesitter-Node)) Child node, or nil.

---

### `Node:named_child()` {#Node-named_child}

```lua
Node:named_child({index})
```

Returns the named child at position {index} (0-based), skipping anonymous nodes.
Returns nil if {index} is out of bounds.

**Parameters:**

- `{index}` (`integer`) 0-based named child index.

**Returns:** ([`Node|nil`](#n00n-treesitter-Node)) Named child node, or nil.

---

### `Node:child_count()` {#Node-child_count}

```lua
Node:child_count()
```

Returns the total number of children, including anonymous nodes.

**Returns:** (`integer`) Child count.

---

### `Node:named_child_count()` {#Node-named_child_count}

```lua
Node:named_child_count()
```

Returns the number of named children (skipping anonymous punctuation nodes).

**Returns:** (`integer`) Named child count.

---

### `Node:children()` {#Node-children}

```lua
Node:children()
```

Returns all children (named and anonymous) as a Lua table.

**Returns:** (`table`) Array of Node.

**Example:**

```lua
for _, child in ipairs(node:children()) do
  print(child:type())
end
```

---

### `Node:named_children()` {#Node-named_children}

```lua
Node:named_children()
```

Returns all named children as a Lua table, skipping anonymous nodes.

**Returns:** (`table`) Array of Node.

---

### `Node:iter_children()` {#Node-iter_children}

```lua
Node:iter_children()
```

Returns an iterator function that yields `(child, field_name)` for every child.
The field name is nil for children that are not assigned to a grammar field.

**Returns:** (`function`) Iterator yielding (Node, string|nil).

**Example:**

```lua
for child, field in node:iter_children() do
  if field then print(field .. ": " .. child:type()) end
end
```

---

### `Node:field()` {#Node-field}

```lua
Node:field({name})
```

Returns all children assigned to the grammar field {name} as a table.
For example, a function node might have a `"name"` or `"body"` field.

**Parameters:**

- `{name}` (`string`) Field name defined in the grammar.

**Returns:** (`table`) Array of Node.

**Example:**

```lua
local bodies = node:field("body")
```

---

### `Node:parent()` {#Node-parent}

```lua
Node:parent()
```

Returns the parent of this node, or nil if this is the root.

**Returns:** ([`Node|nil`](#n00n-treesitter-Node)) Parent node.

---

### `Node:next_sibling()` {#Node-next_sibling}

```lua
Node:next_sibling()
```

Returns the next sibling (named or anonymous), or nil if this is the last child.

**Returns:** ([`Node|nil`](#n00n-treesitter-Node)) Next sibling.

---

### `Node:prev_sibling()` {#Node-prev_sibling}

```lua
Node:prev_sibling()
```

Returns the previous sibling (named or anonymous), or nil if this is the first child.

**Returns:** ([`Node|nil`](#n00n-treesitter-Node)) Previous sibling.

---

### `Node:next_named_sibling()` {#Node-next_named_sibling}

```lua
Node:next_named_sibling()
```

Returns the next named sibling, skipping anonymous nodes. Returns nil at the end.

**Returns:** ([`Node|nil`](#n00n-treesitter-Node)) Next named sibling.

---

### `Node:prev_named_sibling()` {#Node-prev_named_sibling}

```lua
Node:prev_named_sibling()
```

Returns the previous named sibling, skipping anonymous nodes. Returns nil at the start.

**Returns:** ([`Node|nil`](#n00n-treesitter-Node)) Previous named sibling.

---

### `Node:child_with_descendant()` {#Node-child_with_descendant}

```lua
Node:child_with_descendant({descendant})
```

Finds the direct child of this node that contains {descendant}.
Returns nil if {descendant} is not actually inside this node.

**Parameters:**

- `{descendant}` ([`Node`](#n00n-treesitter-Node)) A node that may be a descendant.

**Returns:** ([`Node|nil`](#n00n-treesitter-Node)) Direct child containing the descendant.

---

### `Node:descendant_for_range()` {#Node-descendant_for_range}

```lua
Node:descendant_for_range({start_row}, {start_col}, {end_row}, {end_col})
```

Finds the smallest node inside this node that spans the given point range.
Includes both named and anonymous nodes.

**Parameters:**

- `{start_row}` (`integer`) Start row (0-based).
- `{start_col}` (`integer`) Start column (0-based).
- `{end_row}` (`integer`) End row (0-based).
- `{end_col}` (`integer`) End column (0-based).

**Returns:** ([`Node|nil`](#n00n-treesitter-Node)) Smallest node covering the range, or nil.

---

### `Node:named_descendant_for_range()` {#Node-named_descendant_for_range}

```lua
Node:named_descendant_for_range({start_row}, {start_col}, {end_row}, {end_col})
```

Like `descendant_for_range`, but only considers named nodes.

**Parameters:**

- `{start_row}` (`integer`) Start row (0-based).
- `{start_col}` (`integer`) Start column (0-based).
- `{end_row}` (`integer`) End row (0-based).
- `{end_col}` (`integer`) End column (0-based).

**Returns:** ([`Node|nil`](#n00n-treesitter-Node)) Smallest named node covering the range, or nil.

---

### `Node:named()` {#Node-named}

```lua
Node:named()
```

Returns true if this is a named node (not anonymous punctuation like `,` or `(`).

**Returns:** (`boolean`)

---

### `Node:extra()` {#Node-extra}

```lua
Node:extra()
```

Returns true if this node is an "extra" (like a comment) that can appear anywhere in the grammar.

**Returns:** (`boolean`)

---

### `Node:missing()` {#Node-missing}

```lua
Node:missing()
```

Returns true if this node is "missing", meaning it was inserted by the parser during error recovery.

**Returns:** (`boolean`)

---

### `Node:has_error()` {#Node-has_error}

```lua
Node:has_error()
```

Returns true if this node or any of its descendants contain a syntax error.

**Returns:** (`boolean`)

---

### `Node:has_changes()` {#Node-has_changes}

```lua
Node:has_changes()
```

Returns true if this node has been marked as changed since the last parse.

**Returns:** (`boolean`)

---

### `Node:equal()` {#Node-equal}

```lua
Node:equal({other})
```

Returns true if this node and {other} are the same node in the tree.

**Parameters:**

- `{other}` ([`Node`](#n00n-treesitter-Node)) Node to compare against.

**Returns:** (`boolean`)

---

### `Node:sexpr()` {#Node-sexpr}

```lua
Node:sexpr()
```

Returns the S-expression (lisp-like) string for this node and its children.
Handy for debugging the tree structure.

**Returns:** (`string`) S-expression.

**Example:**

```lua
print(node:sexpr()) -- e.g. "(identifier)"
```

---

### `Node:tree()` {#Node-tree}

```lua
Node:tree()
```

Returns the Tree that this node belongs to.

**Returns:** ([`Tree`](#n00n-treesitter-Tree)) The owning tree.


## n00n.treesitter.LanguageTree {#n00n-treesitter-LanguageTree}

Manages parsing of a source string for a single language.

Obtained from `n00n.treesitter.get_parser()` or `n00n.treesitter.get_string_parser()`.
Call `:parse()` to get the syntax tree, then use `:root()` on the tree to start walking nodes.

```lua
local parser, err = n00n.treesitter.get_parser(source, "lua")
if not err then
  local trees = parser:parse()
  local root = trees[1]:root()
end
```

---

### `LanguageTree:parse()` {#LanguageTree-parse}

```lua
LanguageTree:parse({range?})
```

Parses the source and returns a table containing the resulting Tree.
The tree is cached, so calling this again is cheap.

**Parameters:**

- `{range?}` (`table`) Unused. Accepted for API compatibility.

**Returns:** (`table`) Array with one Tree element.

**Example:**

```lua
local trees = parser:parse()
local root = trees[1]:root()
```

---

### `LanguageTree:lang()` {#LanguageTree-lang}

```lua
LanguageTree:lang()
```

Returns the language name this parser was created with.

**Returns:** (`string`) Language name, e.g. `"lua"`.

---

### `LanguageTree:children()` {#LanguageTree-children}

```lua
LanguageTree:children()
```

Returns child LanguageTrees for injected languages.
Not yet implemented, always returns an empty table.

**Returns:** (`table`) Empty table.

---

### `LanguageTree:trees()` {#LanguageTree-trees}

```lua
LanguageTree:trees()
```

Returns all parsed trees as a table (at most one for now).
Returns an empty table if `parse()` has not been called yet.

**Returns:** (`table`) Array of Tree.

---

### `LanguageTree:source()` {#LanguageTree-source}

```lua
LanguageTree:source()
```

Returns the source string this parser was created with.

**Returns:** (`string`) The original source text.

---

### `LanguageTree:is_valid()` {#LanguageTree-is_valid}

```lua
LanguageTree:is_valid({exclude_children?}, {range?})
```

Checks whether the parse tree is still valid.
Not yet implemented, always returns true.

**Parameters:**

- `{exclude_children?}` (`boolean`) Unused.
- `{range?}` (`table`) Unused.

**Returns:** (`boolean`) Always true.

---

### `LanguageTree:for_each_tree()` {#LanguageTree-for_each_tree}

```lua
LanguageTree:for_each_tree({fn})
```

Calls {fn} with `(tree, nil)` for the parsed tree.
Triggers a parse if the tree has not been parsed yet.

**Parameters:**

- `{fn}` (`function`) Callback receiving `(Tree, nil)`.

**Example:**

```lua
parser:for_each_tree(function(tree, _)
  print(tree:root():type())
end)
```

---

### `LanguageTree:included_regions()` {#LanguageTree-included_regions}

```lua
LanguageTree:included_regions()
```

Returns the regions this parser covers.
Not yet implemented, always returns a table with one empty region.

**Returns:** (`table`) Array with one empty table.

---

### `LanguageTree:contains()` {#LanguageTree-contains}

```lua
LanguageTree:contains({range})
```

Checks whether this parser covers the given {range}.
Not yet implemented, always returns true.

**Parameters:**

- `{range}` (`table`) Range to check (currently unused).

**Returns:** (`boolean`) Always true.

---

### `LanguageTree:destroy()` {#LanguageTree-destroy}

```lua
LanguageTree:destroy()
```

Drops the cached parse tree and frees its memory.
After calling this, the next `parse()` will re-parse from scratch.


## n00n.ui {#n00n-ui}

Functions for building interactive UI. Create buffers to hold
content, open floating or split windows to display them, highlight
code, render markdown, and show status hints.

```lua
local buf = n00n.ui.buf()
buf:line("hello from my plugin!")
local win = n00n.ui.open_win(buf, { title = "Greeting", width = "50%", height = 5 })
```

---

### `n00n.ui.buf()` {#n00n-ui-buf}

```lua
n00n.ui.buf()
```

Creates a new buffer for building UI content. The first buffer you
create in a task becomes the "live" buffer, streamed to the UI while
your tool runs. Create more buffers for secondary content like
floating windows.

**Returns:** ([`Buf`](#n00n-ui-Buf)) Buffer handle.

**Example:**

```lua
local buf = n00n.ui.buf()
buf:line("hello world")
```

---

### `n00n.ui.theme_color()` {#n00n-ui-theme_color}

```lua
n00n.ui.theme_color({name})
```

Looks up a semantic color from the current theme. Use this to keep
your plugin's colors consistent with the rest of the UI.

**Parameters:**

- `{name}` (`string`) Semantic color name, e.g. "accent" or "background".

**Returns:** (`string|nil`) "#rrggbb" hex color, or nil if the name is unknown.

**Example:**

```lua
local accent = n00n.ui.theme_color("accent")
if accent then
  buf:line({ { "note", { fg = accent, bold = true } } })
end
```

---

### `n00n.ui.highlight()` {#n00n-ui-highlight}

```lua
n00n.ui.highlight({code}, {lang}, {opts?})
```

Syntax-highlights a chunk of source code. Returns a table of styled
lines that you can feed into a buffer. Each line is a list of
`{text, style}` spans where style is a `{fg, bold?, italic?, underline?}` table.

**Parameters:**

- `{code}` (`string`) Source text to highlight.
- `{lang}` (`string`) Language identifier, e.g. "rust", "python".
- `{opts?}` (`table?`) Options. Fields:
  - `independent` (`boolean`) highlight each line without cross-line context. Default false.
  - `prefix` (`string`) prepend to the source before highlighting (affects token context). Default "".

**Returns:** (`table`) Lines: `{ { {text, style}, ... }, ... }`. Each style is `{fg, bold?, italic?, underline?}`.

**Example:**

```lua
local lines = n00n.ui.highlight("fn main() {}", "rust")
for _, spans in ipairs(lines) do
  buf:line(spans)
end
```

---

### `n00n.ui.markdown()` {#n00n-ui-markdown}

```lua
n00n.ui.markdown({text}, {width})
```

Renders Markdown into styled lines ready to display in a buffer.
Each span's style is either a named string ("bold", "heading",
"inline_code", etc.) or a `{fg, bold?, italic?, underline?}` table
for syntax-highlighted code blocks.

**Parameters:**

- `{text}` (`string`) Markdown source.
- `{width}` (`integer`) Wrap width in columns.

**Returns:** (`table`) Lines: `{ { {text, style}, ... }, ... }`.

**Example:**

```lua
local size = n00n.ui.terminal_size()
local lines = n00n.ui.markdown("# Hello\n\nSome **bold** text.", size.cols)
for _, spans in ipairs(lines) do
  buf:line(spans)
end
```

---

### `n00n.ui.humantime()` {#n00n-ui-humantime}

```lua
n00n.ui.humantime({secs})
```

Formats a number of seconds into a short, human-friendly string.
Useful for displaying elapsed time in status messages.

**Parameters:**

- `{secs}` (`integer`) Duration in seconds.

**Returns:** (`string`) Human-readable duration, e.g. "1m30s".

**Example:**

```lua
n00n.ui.humantime(90)   -- "1m30s"
n00n.ui.humantime(3661) -- "1h1m1s"
```

---

### `n00n.ui.terminal_size()` {#n00n-ui-terminal_size}

```lua
n00n.ui.terminal_size()
```

Returns the current terminal size. Handy for sizing floating windows
or wrapping text to fit the screen.

**Returns:** (`table`) `{cols, rows}`, terminal width and height in characters.

**Example:**

```lua
local size = n00n.ui.terminal_size()
local half_width = math.floor(size.cols / 2)
```

---

### `n00n.ui.display_width()` {#n00n-ui-display_width}

```lua
n00n.ui.display_width({text})
```

Returns the display width of a string in terminal cells, matching
how `ratatui` measures text.

**Parameters:**

- `{text}` (`string`) The text to measure.

**Returns:** (`integer`) Number of display cells the text occupies.

**Example:**

```lua
local w = n00n.ui.display_width("hello")
```

---

### `n00n.ui.truncate_text()` {#n00n-ui-truncate_text}

```lua
n00n.ui.truncate_text({text}, {max_width})
```

Splits a string at a display-cell boundary.

**Parameters:**

- `{text}` (`string`) The text to split.
- `{max_width}` (`integer`) Maximum display cells for the head.

**Returns:** (`table`) `{head = string, tail = string}`.

**Example:**

```lua
local t = n00n.ui.truncate_text("hello world", 5)
-- t.head == "hello", t.tail == " world"
```

---

### `n00n.ui.flash()` {#n00n-ui-flash}

```lua
n00n.ui.flash({msg})
```

Shows a brief message in the status bar. The message disappears
after a short time. Good for confirming an action like "copied!"
or showing a transient warning.

**Parameters:**

- `{msg}` (`string`) Message text.

**Example:**

```lua
n00n.ui.flash("Copied to clipboard!")
```

---

### `n00n.ui.notify()` {#n00n-ui-notify}

```lua
n00n.ui.notify({msg})
```

Sends a desktop notification carrying {msg} through the terminal. The
mechanism comes from `ui.notifications`: "bell" rings the terminal
bell, "osc9" emits a desktop-notification escape, "all" does both, and
"off" stays silent. Unlike the automatic turn-end signal, an explicit
notify fires even while the terminal is focused.

**Parameters:**

- `{msg}` (`string`) Notification text.

**Example:**

```lua
n00n.ui.notify("Release build finished")
```

---

### `n00n.ui.open_editor()` {#n00n-ui-open_editor}

```lua
n00n.ui.open_editor({path})
```

Opens {path} in the user's `$EDITOR` (e.g. vim, nano) and waits for
it to close. This suspends the TUI while the editor is running.
Returns the editor's exit code so you can check if the user saved.

**Parameters:**

- `{path}` (`string`) File to open.

**Returns:** (`integer`) Editor exit code, or -1 if the action could not be dispatched.

**Example:**

```lua
local code = n00n.ui.open_editor("/tmp/scratch.lua")
if code == 0 then
  n00n.ui.flash("File saved")
end
```

---

### `n00n.ui.pick_model()` {#n00n-ui-pick_model}

```lua
n00n.ui.pick_model({current?})
```

Opens n00n's native model discovery picker and waits for a selection.
Returns nil when the picker is cancelled or the UI is unavailable.

**Parameters:**

- `{current?}` (`string?`) Model spec to preselect. Omit for no preselection.

**Returns:** (`string?`) Selected model spec, or nil when cancelled.

**Example:**

```lua
local model = n00n.ui.pick_model(current_model)
if model then current_model = model end
```

---

### `n00n.ui.open_win()` {#n00n-ui-open_win}

```lua
n00n.ui.open_win({buf}, {opts})
```

Opens a floating or split window that displays the contents of {buf}.
Returns a Win handle you can use to receive events, update layout,
and close the window when you are done.

**Parameters:**

- `{buf}` ([`Buf`](#n00n-ui-Buf)) Buffer to display.
- `{opts}` (`table`) Float configuration. Fields:
  - `width` (`integer|string`) window width. Integer for absolute columns; "N%" for percent of terminal width. Default "60%".
  - `height` (`integer|string`) window height. Integer for absolute rows; "N%" for percent of terminal height. Default "70%".
  - `row` (`integer?`) row offset from the anchor corner. Negative values move up.
  - `col` (`integer?`) column offset from the anchor corner.
  - `anchor` (`string`) corner the (row, col) offset is relative to. One of "NW" (default), "NE", "SW", "SE".
  - `border` (`string`) border style. One of "rounded" (default), "single", "double", "none".
  - `title` (`string`) text shown in the top border. Default "".
  - `title_pos` (`string`) title alignment. One of "left" (default), "center", "right".
  - `footer` (`table`) key-hint pairs shown in the bottom border. Each entry is {key, label}.
  - `zindex` (`integer`) stacking order. Default 50.
  - `cursor_line` (`boolean`) highlight the focused row. Default false.
  - `reserved_top` (`integer`) rows reserved at the top of the content area. Default 0.
  - `reserved_bottom` (`integer`) rows reserved at the bottom of the content area. Default 0.
  - `split` (`string`) dock the window to an edge instead of floating. One of "above", "below", "left", "right", "panel", or "" (floating, default).
  - `order` (`integer`) paint order among split windows at the same edge. Default 50.
  - `focus` (`boolean`) whether the window takes keyboard focus on open. Default true.
  - `visible` (`boolean`) whether the window is initially visible. Default true.

**Returns:** ([`Win`](#n00n-ui-Win)) Window handle.

**Example:**

```lua
local buf = n00n.ui.buf()
buf:line("Pick an option:")
local win = n00n.ui.open_win(buf, {
  title = "Menu",
  width = "50%",
  height = 10,
  cursor_line = true,
  footer = { { "q", "quit" }, { "Enter", "select" } },
})
```

---

### `n00n.ui.set_status_hint()` {#n00n-ui-set_status_hint}

```lua
n00n.ui.set_status_hint({spans})
```

Shows key hints in the status bar for your plugin. Each hint is a {key, label} pair. Pass nil to clear your plugin's hints. Only your own hints are affected, other plugins keep theirs.

**Parameters:**

- `{spans}` (`table|nil`) Sequence of {key, label} pairs, e.g. `{{"q", "quit"}, {"j", "down"}}`. Pass nil to remove the plugin's hints.

**Example:**

```lua
n00n.ui.set_status_hint({ {"q", "quit"}, {"j", "down"} })
-- later, clear them:
n00n.ui.set_status_hint(nil)
```


## n00n.ui.Win {#n00n-ui-Win}

Handle to a floating or split window. You get one from
`n00n.ui.open_win()`. Use `recv()` in a loop to handle keyboard
input, and call `close()` when done.

Fields: `width`, `height` (initial content dimensions in columns/rows),
`visible` (current visibility).

```lua
local win = n00n.ui.open_win(buf, { title = "Demo" })
while true do
  local ev = win:recv()
  if not ev or ev.key == "q" then break end
end
win:close()
```

---

### `Win:recv()` {#Win-recv}

```lua
Win:recv({timeout_ms?})
```

Waits for the next event from this window. Call this in a loop to build an interactive UI. Returns nil once the window is closed or the channel disconnects. Pass {timeout_ms} to also get `{type="timeout"}` events so your plugin can animate while idle.

Event tables by type:
- `{type="key", key}` -- keypress. Key is a string like "q", "j", or "esc".
- `{type="resize", width, height}` -- terminal was resized.
- `{type="paste", text}` -- bracketed paste.
- `{type="close"}` -- window was closed externally.
- `{type="timeout"}` -- no event arrived within {timeout_ms}.

**Parameters:**

- `{timeout_ms?}` (`integer`) Max milliseconds to wait before a timeout event is returned.

**Returns:** (`table|nil`) Event table, or nil if the window has closed.

**Example:**

```lua
while true do
  local ev = win:recv()
  if not ev or ev.key == "q" then break end
  if ev.type == "key" and ev.key == "j" then
    -- move cursor down
  end
end
win:close()
```

---

### `Win:set_config()` {#Win-set_config}

```lua
Win:set_config({opts})
```

Updates the window layout on the fly. Only the fields you include in
{opts} are changed, everything else stays the same.

**Parameters:**

- `{opts}` (`table`) Partial float config. Accepted fields:
  - `title` (`string`) border title text.
  - `title_pos` (`string`) title alignment, "left", "center", or "right".
  - `footer` (`table`) key-hint pairs `{{key, label}, ...}` shown in the bottom border.
  - `border` (`string`) "rounded", "single", "double", or "none".
  - `anchor` (`string`) corner origin, "NW", "NE", "SW", or "SE".
  - `width` (`integer|string`) new width; integer or "N%".
  - `height` (`integer|string`) new height; integer or "N%".
  - `zindex` (`integer`) stacking order.
  - `cursor_line` (`boolean`) highlight the focused row.
  - `reserved_top` (`integer`) rows reserved at the top of the content area.
  - `split` (`string`) edge docking, "above", "below", "left", "right", "panel", or "".
  - `order` (`integer`) paint order among split windows.

**Example:**

```lua
win:set_config({ title = "Updated!", width = "80%" })
```

---

### `Win:set_cursor()` {#Win-set_cursor}

```lua
Win:set_cursor({row})
```

Moves the highlighted cursor line to {row} (1-indexed). Only has a
visible effect when the window was opened with `cursor_line = true`.

**Parameters:**

- `{row}` (`integer`) Target row, 1-indexed.

**Example:**

```lua
win:set_cursor(3) -- highlight the third line
```

---

### `Win:close()` {#Win-close}

```lua
Win:close()
```

Closes the window and frees its resources. Safe to call more than
once. The window also closes automatically when the handle is
garbage collected.

**Example:**

```lua
win:close()
```

---

### `Win:is_open()` {#Win-is_open}

```lua
Win:is_open()
```

Returns true if the window is still alive (not closed). Useful for
checking before sending commands.

**Returns:** (`boolean`) true if open.

**Example:**

```lua
if win:is_open() then
  win:set_config({ title = "still here" })
end
```

---

### `Win:show()` {#Win-show}

```lua
Win:show()
```

Makes the window visible again after it was hidden with `hide()`.

**Example:**

```lua
win:show()
```

---

### `Win:hide()` {#Win-hide}

```lua
Win:hide()
```

Hides the window without closing it. The window keeps its state
and buffer contents. Call `show()` to bring it back.

**Example:**

```lua
win:hide()
-- do some work...
win:show()
```

---

### `Win:is_visible()` {#Win-is_visible}

```lua
Win:is_visible()
```

Returns true if the window is both open and visible (not hidden).

**Returns:** (`boolean`) true if visible.


## n00n.ui.Buf {#n00n-ui-Buf}

A content buffer that holds styled lines of text. Create one with
`n00n.ui.buf()` and pass it to `n00n.ui.open_win()` to show it in
a floating or split window.

```lua
local buf = n00n.ui.buf()
buf:line("hello")
buf:line({ { "world", "bold" } })
```

---

### `Buf:line()` {#Buf-line}

```lua
Buf:line({line})
```

Appends a single line to the end of the buffer. You can pass a
plain string for unstyled text, or a table of `{text, style?}` spans
for rich content. Style can be a named string like "bold" or
"keyword", or an inline table `{fg?, bg?, bold?, italic?, underline?, dim?, strikethrough?, reversed?}`
with "#rrggbb" color strings.

**Parameters:**

- `{line}` (`string|table`) Plain string, or a sequence of spans: `{ {text, style?}, ... }`.

**Example:**

```lua
buf:line("plain text")
buf:line({ { "ERROR", { fg = "#ff0000", bold = true } }, { " something broke" } })
```

---

### `Buf:lines()` {#Buf-lines}

```lua
Buf:lines({lines})
```

Appends several lines at once. Each entry uses the same format as
`buf:line()`, so you can mix plain strings and styled spans.

**Parameters:**

- `{lines}` (`table`) Sequence of line values, each the same format accepted by `buf:line`.

**Example:**

```lua
buf:lines({
  "first line",
  { { "styled ", "bold" }, { "second line" } },
  "third line",
})
```

---

### `Buf:set_lines()` {#Buf-set_lines}

```lua
Buf:set_lines({lines})
```

Replaces every line in the buffer with {lines}. Use this when you
want to redraw the whole buffer, for example after the user toggles
a view.

**Parameters:**

- `{lines}` (`table`) Sequence of line values, each the same format accepted by `buf:line`.

**Example:**

```lua
buf:set_lines({ "new content", "replaces everything" })
```

---

### `Buf:len()` {#Buf-len}

```lua
Buf:len()
```

Returns how many lines the buffer currently holds.

**Returns:** (`integer`) Line count.

**Example:**

```lua
if buf:len() == 0 then
  buf:line("(empty)")
end
```

---

### `Buf:get_lines()` {#Buf-get_lines}

```lua
Buf:get_lines()
```

Returns all lines in the buffer as a Lua table. Each line is a
sequence of `{text, style?}` spans, the same format `buf:line()`
accepts. Useful for reading back content, copying it to another
buffer, or round-tripping through `set_lines()`.

**Returns:** (`table`) Sequence of lines.

**Example:**

```lua
local lines = buf:get_lines()
buf:set_lines(lines) -- round-trip
```

---

### `Buf:on()` {#Buf-on}

```lua
Buf:on({event}, {callback})
```

Registers an event handler on the buffer.

Supported events:
- "click": fires when the user clicks a line. The handler receives
  a click-event table and may yield or mutate the buffer.
- "change": fires synchronously after every mutation (`line`,
  `lines`, `set_lines`). Must not yield.

Calling `on()` again for the same event replaces the previous handler.

**Parameters:**

- `{event}` (`string`) Event name: "click" or "change".
- `{callback}` (`function`) Handler function. For "click", receives a click-event table. For "change", receives no arguments.

**Example:**

```lua
buf:on("click", function(ev)
  n00n.ui.flash("Clicked row " .. ev.row)
end)
```

---

### `Buf:click()` {#Buf-click}

```lua
Buf:click({ev})
```

Programmatically fires the buffer's click handler with event {ev}.
Does nothing if no click handler is registered. Useful for testing
or simulating user interaction from code.

**Parameters:**

- `{ev}` (`table`) Click event table passed to the handler.

**Example:**

```lua
buf:click({ row = 1 })
```

---

### `Buf:blit()` {#Buf-blit}

```lua
Buf:blit({fb}, {width}, {height}, {opts?})
```

Replaces the whole buffer with a pixel frame drawn as `"▀"` cells.
Each cell's foreground is the top pixel and its background the
bottom one, so one text line fits two pixel rows. When {height} is
odd the last line leaves its background unset and the terminal
default shows through.

{fb} is a Luau `buffer` of raw pixel bytes in row-major order,
top-left origin. Its size must be exactly
`width * height * bytes_per_pixel` for the chosen format, otherwise
the call throws. A mismatch usually means a wrong width or format,
and an early error beats hunting down a garbled frame.

Formats: "rgb" is the default at 3 bytes per pixel. "rgba" and
"bgra" take 4 bytes per pixel and ignore the 4th byte. "bgra" is
what a little-endian `uint32` holding `0xRRGGBB` looks like in
memory, the layout doomgeneric uses for its framebuffer.

`char` swaps the `"▀"` glyph for another one column wide string,
e.g. `"█"` when only the foreground color should show. The
foreground still comes from the top pixel and the background from
the bottom one, whatever the glyph.

**Parameters:**

- `{fb}` (`buffer`) Raw pixel bytes.
- `{width}` (`integer`) Frame width in pixels, > 0.
- `{height}` (`integer`) Frame height in pixels, > 0.
- `{opts?}` (`table|nil`) Options: `format` = "rgb"|"rgba"|"bgra", `char` = one column wide string.

**Example:**

```lua
local fb = buffer.create(160 * 100 * 3)
buffer.writeu8(fb, (y * 160 + x) * 3, 255) -- red channel
buf:blit(fb, 160, 100)
buf:blit(fb32, 160, 100, { format = "bgra", char = "█" })
```


## n00n.uv {#n00n-uv}

System and environment utilities, modelled after `vim.uv`.

Provides access to the working directory, home directory, and environment
variables. None of these functions throw.

```lua
local home = n00n.uv.os_homedir()
```

---

### `n00n.uv.cwd()` {#n00n-uv-cwd}

```lua
n00n.uv.cwd()
```

Return the current working directory as an absolute path. Like `vim.uv.cwd`.

**Returns:** (`string?`) Current working directory, or nil if it cannot be determined.

**Example:**

```lua
local cwd = n00n.uv.cwd()
if cwd then print("working in: " .. cwd) end
```

---

### `n00n.uv.os_homedir()` {#n00n-uv-os_homedir}

```lua
n00n.uv.os_homedir()
```

Return the current user's home directory. Like `vim.uv.os_homedir`.

**Returns:** (`string?`) Home directory path, or nil if it cannot be determined.

**Example:**

```lua
local home = n00n.uv.os_homedir() -- e.g. "/home/user"
```

---

### `n00n.uv.current_exe()` {#n00n-uv-current_exe}

```lua
n00n.uv.current_exe()
```

Return the path of the running n00n executable.

**Returns:** (`string?`) Executable path, or nil if it cannot be determined.

**Example:**

```lua
local n00n_bin = n00n.uv.current_exe()
```

---

### `n00n.uv.os_getenv()` {#n00n-uv-os_getenv}

```lua
n00n.uv.os_getenv({name})
```

Look up the environment variable {name}. Like `vim.uv.os_getenv`.
Returns nil when the variable is not set.

**Parameters:**

- `{name}` (`string`) Name of the environment variable.

**Returns:** (`string?`) Variable value, or nil if not set.

**Example:**

```lua
local editor = n00n.uv.os_getenv("EDITOR") or "vi"
```


## n00n.codegraph {#n00n-codegraph}

Cross-file structural exploration via native `.codegraph/codegraph.db` queries with CLI fallback.

---

### `n00n.codegraph.check_binary()` {#n00n-codegraph-check_binary}

```lua
n00n.codegraph.check_binary()
```

Check that the `codegraph` CLI is installed and working.

**Returns:** (`boolean`, `string?`) ok and optional error message.

---

### `n00n.codegraph.available()` {#n00n-codegraph-available}

```lua
n00n.codegraph.available()
```

Returns true if the `codegraph` CLI is on PATH.

**Returns:** (`boolean`) true when codegraph is available.

---

### `n00n.codegraph.has_index()` {#n00n-codegraph-has_index}

```lua
n00n.codegraph.has_index({project})
```

Returns true when `.codegraph/` exists in the project root.

**Parameters:**

- `{project}` (`string`) Path to the project root.

**Returns:** (`boolean`) true when a codegraph index is present.

---

### `n00n.codegraph.has_database()` {#n00n-codegraph-has_database}

```lua
n00n.codegraph.has_database({project})
```

Returns true when `.codegraph/codegraph.db` exists in the project root.

**Parameters:**

- `{project}` (`string`) Path to the project root.

**Returns:** (`boolean`) true when the native SQLite index is present.

---

### `n00n.codegraph.explore()` {#n00n-codegraph-explore}

```lua
n00n.codegraph.explore({query}, {project}, {timeout_secs?})
```

Run an explore query using the native SQLite index when available, otherwise `codegraph explore`.

**Parameters:**

- `{query}` (`string`) Natural language question or symbol names to explore.
- `{project}` (`string`) Path to the project root.
- `{timeout_secs}` (`integer`) Optional timeout in seconds (default 30).

**Returns:** (`string?`, `string?`) output and optional error message.

---

### `n00n.codegraph.callers()` {#n00n-codegraph-callers}

```lua
n00n.codegraph.callers({symbol}, {project}, {timeout_secs?})
```

Find all functions/methods that call a specific symbol using native SQLite when available, otherwise `codegraph callers`.

**Parameters:**

- `{symbol}` (`string`) Symbol name to find callers for.
- `{project}` (`string`) Path to the project root.
- `{timeout_secs}` (`integer`) Optional timeout in seconds (default 30).

**Returns:** (`string?`, `string?`) output and optional error message.

---

### `n00n.codegraph.callees()` {#n00n-codegraph-callees}

```lua
n00n.codegraph.callees({symbol}, {project}, {timeout_secs?})
```

Find all functions/methods that a specific symbol calls using native SQLite when available, otherwise `codegraph callees`.

**Parameters:**

- `{symbol}` (`string`) Symbol name to find callees for.
- `{project}` (`string`) Path to the project root.
- `{timeout_secs}` (`integer`) Optional timeout in seconds (default 30).

**Returns:** (`string?`, `string?`) output and optional error message.

---

### `n00n.codegraph.impact()` {#n00n-codegraph-impact}

```lua
n00n.codegraph.impact({symbol}, {project}, {timeout_secs?})
```

Analyze what code is affected by changing a symbol using native SQLite when available, otherwise `codegraph impact`.

**Parameters:**

- `{symbol}` (`string`) Symbol name to analyze impact for.
- `{project}` (`string`) Path to the project root.
- `{timeout_secs}` (`integer`) Optional timeout in seconds (default 30).

**Returns:** (`string?`, `string?`) output and optional error message.

---

### `n00n.codegraph.affected()` {#n00n-codegraph-affected}

```lua
n00n.codegraph.affected({files}, {project}, {timeout_secs?})
```

Accept an array of file paths and compute the affected file set using `codegraph affected`.

**Parameters:**

- `{files}` (`table<string>`) Array of file paths that changed.
- `{project}` (`string`) Path to the project root.
- `{timeout_secs}` (`integer`) Optional timeout in seconds (default 30).

**Returns:** (`string?`, `string?`) output and optional error message.

---

### `n00n.codegraph.node()` {#n00n-codegraph-node}

```lua
n00n.codegraph.node({name}, {project}, {timeout_secs?})
```

Get one symbol's source location and signature using native SQLite when available, otherwise `codegraph node` (which may include a caller/callee trail).

**Parameters:**

- `{name}` (`string`) Symbol name to look up.
- `{project}` (`string`) Path to the project root.
- `{timeout_secs}` (`integer`) Optional timeout in seconds (default 30).

**Returns:** (`string?`, `string?`) output and optional error message.

---

### `n00n.codegraph.query()` {#n00n-codegraph-query}

```lua
n00n.codegraph.query({search}, {project}, {timeout_secs?})
```

Search for symbols in the codebase using native SQLite when available, otherwise `codegraph query`.

**Parameters:**

- `{search}` (`string`) Search query for symbols.
- `{project}` (`string`) Path to the project root.
- `{timeout_secs}` (`integer`) Optional timeout in seconds (default 30).

**Returns:** (`string?`, `string?`) output and optional error message.

---

### `n00n.codegraph.sync()` {#n00n-codegraph-sync}

```lua
n00n.codegraph.sync({project}, {timeout_secs?})
```

Sync changes since last index using `codegraph sync`.

**Parameters:**

- `{project}` (`string`) Path to the project root.
- `{timeout_secs}` (`integer`) Optional timeout in seconds (default 30).

**Returns:** (`string?`, `string?`) output and optional error message.

---

### `n00n.codegraph.files()` {#n00n-codegraph-files}

```lua
n00n.codegraph.files({project}, {timeout_secs?})
```

Show project file structure from the index using native SQLite when available, otherwise `codegraph files`.

**Parameters:**

- `{project}` (`string`) Path to the project root.
- `{timeout_secs}` (`integer`) Optional timeout in seconds (default 30).

**Returns:** (`string?`, `string?`) output and optional error message.


## n00n.git {#n00n-git}

In-process access to the git operations linked into n00n.

---

### `n00n.git.run()` {#n00n-git-run}

```lua
n00n.git.run({command}, {repo}, {options?})
```

Run a bundled git operation and return its JSON result.

**Parameters:**

- `{command}` (`string`) Operation name.
- `{repo}` (`string`) Path to the repository.
- `{options}` (`table`) Operation-specific arguments.

**Returns:** (`string|nil`, `string|nil`) JSON result, or nil and the error message.


## n00n.github {#n00n-github}

GitHub REST API client using reqwest. Provides structured access to GitHub issues, pull requests, and repository metadata. Token sources: GITHUB_TOKEN env var, optional token parameter, or gh CLI fallback.

---

### `n00n.github.list_issues()` {#n00n-github-list_issues}

```lua
n00n.github.list_issues({owner}, {repo}[, {token}])
```

List issues in a GitHub repository.

**Parameters:**

- `{owner}` (`string`) Repository owner (username or organization).
- `{repo}` (`string`) Repository name.
- `{token}` (`string?`) Optional GitHub token. Falls back to GITHUB_TOKEN env var or gh CLI.

**Returns:** (`table`) Array of issue objects with number, title, state, user, body, and html_url.

---

### `n00n.github.create_issue()` {#n00n-github-create_issue}

```lua
n00n.github.create_issue({owner}, {repo}, {title}[, {body}[, {token}]])
```

Create a new issue in a GitHub repository. Requires authentication.

**Parameters:**

- `{owner}` (`string`) Repository owner (username or organization).
- `{repo}` (`string`) Repository name.
- `{title}` (`string`) Issue title.
- `{body}` (`string?`) Issue body (markdown). Optional.
- `{token}` (`string?`) Optional GitHub token. Falls back to GITHUB_TOKEN env var or gh CLI.

**Returns:** (`table`) Created issue object with number, title, state, user, body, and html_url.

---

### `n00n.github.list_prs()` {#n00n-github-list_prs}

```lua
n00n.github.list_prs({owner}, {repo}[, {token}])
```

List pull requests in a GitHub repository.

**Parameters:**

- `{owner}` (`string`) Repository owner (username or organization).
- `{repo}` (`string`) Repository name.
- `{token}` (`string?`) Optional GitHub token. Falls back to GITHUB_TOKEN env var or gh CLI.

**Returns:** (`table`) Array of pull request objects with number, title, state, user, head, base, body, and html_url.

---

### `n00n.github.get_repo()` {#n00n-github-get_repo}

```lua
n00n.github.get_repo({owner}, {repo}[, {token}])
```

Get repository metadata from GitHub.

**Parameters:**

- `{owner}` (`string`) Repository owner (username or organization).
- `{repo}` (`string`) Repository name.
- `{token}` (`string?`) Optional GitHub token. Falls back to GITHUB_TOKEN env var or gh CLI.

**Returns:** (`table`) Repository object with name, full_name, description, language, stargazers_count, forks_count, and html_url.

---

### `n00n.github.get_issue()` {#n00n-github-get_issue}

```lua
n00n.github.get_issue({owner}, {repo}, {issue_number}[, {token}])
```

Get a single issue from GitHub.

**Parameters:**

- `{owner}` (`string`) Repository owner (username or organization).
- `{repo}` (`string`) Repository name.
- `{issue_number}` (`integer`) Issue number.
- `{token}` (`string?`) Optional GitHub token. Falls back to GITHUB_TOKEN env var or gh CLI.

**Returns:** (`table`) Issue object with number, title, state, user, body, and html_url.

---

### `n00n.github.get_pr()` {#n00n-github-get_pr}

```lua
n00n.github.get_pr({owner}, {repo}, {pr_number}[, {token}])
```

Get a single pull request from GitHub.

**Parameters:**

- `{owner}` (`string`) Repository owner (username or organization).
- `{repo}` (`string`) Repository name.
- `{pr_number}` (`integer`) Pull request number.
- `{token}` (`string?`) Optional GitHub token. Falls back to GITHUB_TOKEN env var or gh CLI.

**Returns:** (`table`) Pull request object with number, title, state, user, head, base, body, and html_url.

---

### `n00n.github.create_pr()` {#n00n-github-create_pr}

```lua
n00n.github.create_pr({owner}, {repo}, {head}, {base}, {title}[, {body}[, {token}]])
```

Create a new pull request in a GitHub repository. Requires authentication.

**Parameters:**

- `{owner}` (`string`) Repository owner (username or organization).
- `{repo}` (`string`) Repository name.
- `{head}` (`string`) The name of the branch where your changes are implemented.
- `{base}` (`string`) The name of the branch you want the changes pulled into.
- `{title}` (`string`) Pull request title.
- `{body}` (`string?`) Pull request body (markdown). Optional.
- `{token}` (`string?`) Optional GitHub token. Falls back to GITHUB_TOKEN env var or gh CLI.

**Returns:** (`table`) Created pull request object with number, title, state, and html_url.

---

### `n00n.github.add_comment()` {#n00n-github-add_comment}

```lua
n00n.github.add_comment({owner}, {repo}, {issue_number}, {body}[, {token}])
```

Add a comment to an issue or pull request. Requires authentication.

**Parameters:**

- `{owner}` (`string`) Repository owner (username or organization).
- `{repo}` (`string`) Repository name.
- `{issue_number}` (`integer`) Issue or pull request number.
- `{body}` (`string`) Comment body (markdown).
- `{token}` (`string?`) Optional GitHub token. Falls back to GITHUB_TOKEN env var or gh CLI.

**Returns:** (`table`) Created comment object with id and html_url.


## n00n.semblem {#n00n-semblem}

BM25 code search and related-chunk lookup via the native `.n00n/search/` index.

---

### `n00n.semblem.has_index()` {#n00n-semblem-has_index}

```lua
n00n.semblem.has_index({project})
```

Returns true when `.n00n/search/metadata.json` exists in the project root.

**Parameters:**

- `{project}` (`string`) Path to the project root.

**Returns:** (`boolean`) true when a search index is present.

---

### `n00n.semblem.search()` {#n00n-semblem-search}

```lua
n00n.semblem.search({repo}, {query}, {mode?}, {top_k?}, {content?})
```

Search indexed source chunks. BM25 is native; hybrid/semantic try the upstream semble CLI and fall back to BM25 with an embedder nag if the CLI is unavailable.

**Parameters:**

- `{repo}` (`string`) Local project root path, or an HTTPS git URL allowed by N00N_SEMBLE_ALLOWED_REMOTE_REPOS.
- `{query}` (`string`) Natural-language or keyword query.
- `{mode}` (`string`) One of bm25, hybrid, or semantic.
- `{top_k}` (`integer`) Maximum number of results.
- `{content}` (`string`) Content filter: docs, config, code, or all.

**Returns:** (`string`) Ranked snippet output.

---

### `n00n.semblem.find_related()` {#n00n-semblem-find_related}

```lua
n00n.semblem.find_related({repo}, {file_path}, {line}, {top_k?})
```

Find chunks related to a file location using BM25 over the anchor chunk.

**Parameters:**

- `{repo}` (`string`) Local project root path, or an HTTPS git URL allowed by N00N_SEMBLE_ALLOWED_REMOTE_REPOS.
- `{file_path}` (`string`) Relative or absolute file path.
- `{line}` (`integer`) 1-based line number inside the file.
- `{top_k}` (`integer`) Maximum number of results.

**Returns:** (`string`) Ranked snippet output.

---

### `n00n.semblem.savings()` {#n00n-semblem-savings}

```lua
n00n.semblem.savings({repo})
```

Estimate token savings from using a hybrid/semantic embedder. Requires the semble CLI and has no native fallback.

**Parameters:**

- `{repo}` (`string`) Local project root path, or an HTTPS git URL allowed by N00N_SEMBLE_ALLOWED_REMOTE_REPOS.

**Returns:** (`string?`, `string?`) savings summary and optional error message.


## n00n.smell {#n00n-smell}

Persistent code-smell and comment index built into n00n. Stores TODO/FIXME/HACK comments and placeholder phrases in a local `.n00n/smells` Tantivy index.

---

### `n00n.smell.has_index()` {#n00n-smell-has_index}

```lua
n00n.smell.has_index({project})
```

Returns true when `.n00n/smells/metadata.json` exists in the project root.

**Parameters:**

- `{project}` (`string`) Path to the project root.

**Returns:** (`boolean`) true when a smell index is present.

---

### `n00n.smell.index()` {#n00n-smell-index}

```lua
n00n.smell.index({project})
```

Build or rebuild the smell index for a repository.

**Parameters:**

- `{project}` (`string`) Path to the project root.

**Returns:** (`boolean`, `string|nil`) true on success, or false and the error message.

---

### `n00n.smell.search()` {#n00n-smell-search}

```lua
n00n.smell.search({project}, {query}, {kind?}, {top_k?})
```

Search the smell index by keyword and optional kind.

**Parameters:**

- `{project}` (`string`) Path to the project root.
- `{query}` (`string`) Keyword or phrase.
- `{kind}` (`string`) Optional kind filter: todo, fixme, hack, placeholder.
- `{top_k}` (`integer`) Maximum number of results (default 5).

**Returns:** (`string|nil`, `string|nil`) Ranked smell output, or nil and the error message.


## n00n.workflow {#n00n-workflow}

Sandboxed workflow script compilation.

Plugins cannot reach Lua's `load`, so this compiles a workflow script
with a caller-supplied environment table, keeping the script inside the
primitives the plugin injects.

```lua
local fn, err = n00n.workflow.compile("return 1 + 1", {})
local key = n00n.workflow.hash("stable payload")
```

---

### `n00n.workflow.compile()` {#n00n-workflow-compile}

```lua
n00n.workflow.compile({source}, {env})
```

Compile {source} into a function whose global environment is exactly {env}.
The chunk sees only the keys you put in {env}: anything else (n00n, os, io,
require, print) reads as nil, so a workflow script stays inside the
primitives the plugin injects. Returns (function, nil) on success, or
(nil, error) when the source fails to compile.

**Parameters:**

- `{source}` (`string`) Lua source to compile.
- `{env}` (`table`) The chunk's global environment.

**Returns:** (`function|nil`, `string|nil`) The compiled chunk, or the compile error.

**Example:**

```lua
local fn, err = n00n.workflow.compile("return agent({ prompt = 'hi' })", { agent = agent })
if fn then print(fn()) end
```

---

### `n00n.workflow.hash()` {#n00n-workflow-hash}

```lua
n00n.workflow.hash({data})
```

SHA-256 hex digest of {data}. Used by the workflow plugin for journal keys
and run ids so identical agent opts collide only on a full 256-bit space.

**Parameters:**

- `{data}` (`string`) Bytes to hash (Lua string, treated as UTF-8 bytes).

**Returns:** (`string`) Lowercase hex SHA-256 digest.

**Example:**

```lua
local k = n00n.workflow.hash("prompt=hi")
```


## n00n.yaml {#n00n-yaml}

YAML encoding and decoding. Works the same way as `n00n.json`,
but for YAML formatted strings.

```lua
local t = n00n.yaml.decode("greeting: hello")
print(t.greeting)
```

---

### `n00n.yaml.encode()` {#n00n-yaml-encode}

```lua
n00n.yaml.encode({value})
```

Turn a Lua value into a YAML string. Most Lua types work, but
circular references will return an error.

**Parameters:**

- `{value}` (`any`) Lua value to encode.

**Returns:** (`string?`, `string?`) YAML string, or nil plus an error.

**Example:**

```lua
local s, err = n00n.yaml.encode({ name = "n00n", tags = { "ai", "agent" } })
print(s)
```

---

### `n00n.yaml.decode()` {#n00n-yaml-decode}

```lua
n00n.yaml.decode({str})
```

Parse a YAML string into a Lua value. Mappings become tables and
sequences become 1-indexed arrays.

**Parameters:**

- `{str}` (`string`) YAML string to decode.

**Returns:** (`any?`, `string?`) Decoded value, or nil plus an error.

**Example:**

```lua
local t, err = n00n.yaml.decode("name: n00n\nversion: 1")
print(t.name) -- n00n
```


## Shared helper modules

These ship inside n00n; `require` them from any plugin. Small modules are
shown as full source, larger ones as their public interface.

### `require("n00n.activity_preview")`

```lua
function ActivityPreview.new(ctx, description, opts)
function ActivityPreview:render()
function ActivityPreview:set_row(key, label, message, status)
function ActivityPreview:update(progress, label, session_key)
function ActivityPreview:prompt(sess, message, label)
```

### `require("n00n.checkpoint")`

```lua
-- Checkpoint: save/load JSON snapshots for run lifecycle.
function M.save(run_id, checkpoint_id, state, sequence)
function M.load(run_id, checkpoint_id)
function M.list(run_id)
function M.latest(run_id)
function M.prune(run_id, keep_n)
```

### `require("n00n.color")`

```lua
local M = {}

function M.lerp(from, to, t)
  local fr, fg, fb = from:match("#(%x%x)(%x%x)(%x%x)")
  local tr, tg, tb = to:match("#(%x%x)(%x%x)(%x%x)")
  if not fr or not tr then
    return from
  end
  fr, fg, fb = tonumber(fr, 16), tonumber(fg, 16), tonumber(fb, 16)
  tr, tg, tb = tonumber(tr, 16), tonumber(tg, 16), tonumber(tb, 16)
  local r = math.floor(fr + (tr - fr) * t + 0.5)
  local g = math.floor(fg + (tg - fg) * t + 0.5)
  local b = math.floor(fb + (tb - fb) * t + 0.5)
  return string.format("#%02x%02x%02x", r, g, b)
end

function M.dim(color, factor)
  local bg = n00n.ui.theme_color("background") or "#000000"
  return M.lerp(color, bg, factor)
end

return M
```

### `require("n00n.explore_result")`

```lua
function Card:update(output)
function ExploreResult.new(opts)
function ExploreResult.live(ctx, opts)
function ExploreResult.header(label, project)
function ExploreResult.restore(output, ctx, opts)
```

### `require("n00n.fuzzy_replace")`

```lua
M.NO_MATCH = "old_string not found in file"
M.MULTIPLE_MATCHES = "old_string matches multiple locations; add surrounding context to make it unique"
M.EMPTY_OLD_STRING = "old_string must not be empty"

-- Replace {old_string} with {new_string} in {content}, tolerating small
-- whitespace and indentation drift. Returns the new content, or nil plus
-- one of the error constants above.
function M.replace(content, old_string, new_string, replace_all)

-- Expose unescape for validation in edit tools
M.unescape = unescape
```

### `require("n00n.guard")`

```lua
-- Runaway guard for subagent budgets.
--
-- Combines a user-configurable call limit with heuristic runaway detection:
-- repeated identical prompts, consecutive subagent errors, and wall-clock timeouts.
--
-- Use as a drop-in replacement for a simple { consume = ... } budget table:
--   guard.consume() is still called before a call.
--   guard.observe(prompt, err) is called after a call if available.
--
-- For richer control, subagent.launch also supports guard:check(prompt) before a
-- call and guard:record(prompt, err) after a call, which lets the guard see the
-- prompt and the result.
function M.new(opts)
```

### `require("n00n.html")`

```lua
--- Convert HTML to compact text while omitting script, style, and noscript content.
function M.strip(html)
```

### `require("n00n.list_picker")`

```lua
-- Open a fuzzy-filter picker in a floating window and block until the user
-- decides. {items} is a list of strings or { label, detail? } tables. {opts}:
-- title, footer, cursor (initial index), submit_keys (extra submit keys
-- besides enter). Returns { type = "choice"|"delete", index } or
-- { type = "close" }.
function ListPicker.open(items, opts)
ListPicker.split_words = split_words
ListPicker.matches = matches
ListPicker.highlight_spans = highlight_spans
```

### `require("n00n.live_context")`

```lua
-- Live context: combine n00n.session.live() + blackboard query for visibility.
function M.snapshot(ctx)
```

### `require("n00n.output_limits")`

```lua
-- Shared per-tool output limit options, so the tools that support them
-- cannot drift apart.
M.DEFAULT_MAX_OUTPUT_LINES = DEFAULT_MAX_OUTPUT_LINES
M.DEFAULT_MAX_LINE_BYTES = DEFAULT_MAX_LINE_BYTES
M.EXPLORER_DEFAULT_MAX_OUTPUT_BYTES = EXPLORER_DEFAULT_MAX_OUTPUT_BYTES
function M.extend(spec)

--- Returns max_lines, max_bytes: tool override when set, agent-wide otherwise.
function M.resolve(opts, ctx)
function M.resolve_capped(opts, ctx, default_max_bytes)
```

### `require("n00n.policy")`

```lua
-- Policy enforcement wrapper for tool calls.
M.canonical_tool_name = canonical_tool_name
function M.evaluate_policy(agent_id, session_type, tags, tool_name)
function M.call_tool(ctx, agent_id, session_type, tags, tool_name, input)
```

### `require("n00n.policy_store")`

```lua
function M.load(path)
```

### `require("n00n.route_tier")`

```lua
-- Cost-aware model-tier router (OrchMAS-style adaptive role allocation).
-- Pure lexical heuristic: no model call. Maps a subtask prompt to one of
-- "weak" | "medium" | "strong" so cheap work stays cheap and hard work
-- gets a bigger model. Used by the `run_task` tool (opt-in auto_tier) and Team.

-- @param prompt string Subtask description.
-- @return "weak" | "medium" | "strong"
function M.route_tier(prompt)
```

### `require("n00n.secret_check")`

```lua
-- Heuristic secret/PII pattern detection for tool content validation.
--
-- Example:
--
--     local secret_check = require("n00n.secret_check")
--     local reason = secret_check.reason("api_key=sk_test_abcdefghijklmnopqrstuvwxyz")
--     if reason then error(reason) end
--
-- This is intentionally conservative: it flags common secret-bearing keywords and
-- patterns so tools can surface a warning or require a justification. It does not
-- attempt to be exhaustive, and it may false-positive on example keys in docs.

-- Returns (ok, reason). If ok is false, reason explains what triggered.
function M.check(text)

-- Convenience: returns a warning string if triggered, nil otherwise.
function M.reason(text)
```

### `require("n00n.shorten_path")`

```lua
local function normalize_sep(s)
  return s:gsub("\\", "/")
end

local function shorten_path(path)
  local p = normalize_sep(path)
  local cwd = n00n.uv.cwd()
  if cwd then
    cwd = normalize_sep(cwd)
    if p:sub(1, #cwd + 1) == cwd .. "/" then
      local rel = p:sub(#cwd + 2)
      return rel == "" and "." or rel
    end
  end
  local home = n00n.uv.os_homedir()
  if home then
    home = normalize_sep(home)
    if p:sub(1, #home + 1) == home .. "/" then
      local rel = p:sub(#home + 2)
      return rel == "" and "~" or "~/" .. rel
    end
  end
  return path
end

return shorten_path
```

### `require("n00n.structured_output")`

```lua
-- Structured output helper module for subagent validation.
-- Provides constants, schema validation, and local tool creation for
-- structured output patterns used across task, workflow, and subagent plugins.

-- Constants
M.STRUCTURED_OUTPUT_NAME = "structured_output"
M.STRUCTURED_OUTPUT_DESCRIPTION = "Report your final result. Call it exactly once when your task is complete."
M.STRUCTURED_OUTPUT_ACK = "Output recorded."
M.STRUCTURED_OUTPUT_SUFFIX = "\n\nWhen finished, call the structured_output tool with your final result."
M.MAX_STRUCTURED_RETRIES = 1
M.MAX_SCHEMA_ERRORS = 3
M.MAX_SCHEMA_BYTES = 32 * 1024
M.MAX_SCHEMA_DEPTH = 16
M.SCHEMA_ROOT_ERROR = "output_schema must have type object"
M.SCHEMA_COMPILE_ERROR = "invalid output_schema"
M.SCHEMA_SIZE_ERROR = "output_schema exceeds 32768-byte limit"
M.SCHEMA_DEPTH_ERROR = "output_schema exceeds maximum depth of 16"
M.STRUCTURED_MISSING_ERROR = "subagent finished without calling structured_output"
M.STRUCTURED_INVALID_ERROR = "subagent result does not match output_schema"
M.INVALID_INPUT_PREFIX = "Input does not match the required schema. Fix the errors and call structured_output again:\n"

-- Check if a schema value is within the maximum depth limit
function M.schema_within_depth(value, depth)

-- Limit error messages to a reasonable number
function M.bounded_errors(errors)

-- Compile a schema validator with early validation checks
-- Returns (validator | nil, err)
function M.compile_validator(schema)

-- Create a local tool spec for structured output
-- Returns a table with description, input_schema, and handler
function M.make_local_tool(schema, on_submit)
```

### `require("n00n.subagent")`

```lua
-- Subagent launch helper module.
-- Provides a unified interface for launching subagents with model resolution,
-- system prompts, tool setup, and optional structured output validation.

-- Return a fresh table containing the orchestration tool names.
-- Use it as a denylist when child agents must not launch more orchestration.
-- Example: local excluded = subagent.orchestration_tools()
-- @return string[]
function M.orchestration_tools()

-- Launch a subagent with the given options.
-- Returns (result | nil, err, cost, usage, model_spec)
--
-- Options:
--   description (required): Short description for the subagent
--   prompt (required): The prompt to send to the subagent
--   subagent_type: "research" or "general" (default: "general")
--   model_spec: Exact model spec (optional)
--   model_tier: Capped tier: "weak", "medium", or "strong"
--   auto_tier: Pick model_tier from prompt automatically (optional)
--   thinking: Thinking mode configuration
--   system: Override the default system prompt (optional)
--   output_schema: JSON Schema for structured output validation
--   audience: Tool audience (default: computed from subagent_type)
--   include_mcp: Include MCP tools (default: true)
--   only_tools: Optional allowlist of tool names
--   except_tools: Optional denylist of tool names
--   allow_orchestration: Expose recursive orchestration tools (default: false)
--   system_append: Trusted instruction appended to the system prompt
--   local_tools: Additional local tools to register
--   preview: ActivityPreview object wrapping sess:prompt (optional)
--   activity_label: Label used with preview (default: description)
--   budget: Budget object with :consume() method (optional)
--   fail_on_pricing_error: Return an error if usage pricing fails (default: false)
--   ctx: Agent context (required)
function M.launch(ctx, opts)
```

### `require("n00n.telemetry")`

```lua
-- Append-only JSONL telemetry logger for multi-agent runs.
function M.open(log_dir, run_id)
```

### `require("n00n.text_input")`

```lua
-- TextInput: multi-line editable buffer with a byte-offset cursor.
--
-- Invariants enforced everywhere:
--   * `line` is 1-based and indexes a line that always exists.
--   * `col` is a byte offset inside `lines[line]`, always on a UTF-8 codepoint
--     boundary, so `lines[line]:sub(1, col)` is a complete UTF-8 prefix.
--   * No line ever contains a literal newline; newlines split into rows.
--
-- Parents OWN their keys. `handle_key` returns one of R.IGNORED / R.MOVED /
-- R.CHANGED. Parent dispatchers must filter their own keys (esc, ctrl+c,
-- submit keys, etc.) BEFORE forwarding, because `handle_key` claims any key
-- it can interpret. `ctrl+a` is bound to move-home; if a parent wants it for
-- "select all" it must intercept first.
--
-- IGNORED is returned when the buffer literally cannot act (backspace at
-- (1, 0), right at end of buffer, etc.). Parents can use that signal to fall
-- through to their own logic.
--
-- Parity cases live in plugins/lib/tests/spec.lua (TRACE_CASES). Add one
-- whenever you change handle_key semantics.
TextInput.Result = R
function TextInput.new()
function TextInput:value()
function TextInput:is_empty()
function TextInput:line_count()
function TextInput:clear()

-- Returns the codepoint right before the cursor as a string, or nil at the
-- start of a line. Lets callers peek backwards (e.g. "is the previous char
-- a backslash?") without touching internal indices.
function TextInput:char_before_cursor()
function TextInput:insert_text(text)
function TextInput:insert_char(c)
function TextInput:insert_space()
function TextInput:split_line()
function TextInput:remove_char()
function TextInput:delete_char()
function TextInput:remove_word_before()
function TextInput:delete_word_after()
function TextInput:kill_to_end_of_line()
function TextInput:move_left()
function TextInput:move_right()
function TextInput:move_up()
function TextInput:move_down()
function TextInput:move_home()
function TextInput:move_end()
function TextInput:move_word_left()
function TextInput:move_word_right()
function TextInput:handle_key(key)

-- Wrap lines to {width} with {prefix} before the first row. Returns
-- { lines = styled lines, cursor_row = 1-based row holding the cursor }.
function TextInput:render(prefix, prefix_width, width)
```

### `require("n00n.tool_view")`

```lua
-- The shared truncate/expand body that tool plugins render through.
--
-- Click handlers get `ev.row`, a 1-based line in this buf; 0 means the
-- click landed outside it (the header). The handler lives on the buf
-- itself, so any wrapper of the same buf (a batch child's foreign handle)
-- reaches the same toggle. Expansion is never stored: the UI records
-- clicked rows and replays them through `restore` in order, so `toggle`
-- stays a pure flag flip + re-render, deterministic across replays.
-- Async highlighting goes through `n00n.async.run`; during restore the
-- runtime runs those tasks inline before snapshotting.

-- opts: max_lines (default 80) shown while collapsed, keep "head"|"tail"
-- (default "tail"), max_expand_lines (default 2000) kept for expansion,
-- max_line_bytes (optional) per-line byte cap applied at render time,
-- max_width (optional) display-width cap, hide_collapsed (default false),
-- header_until_blank keeps a leading metadata block outside the content limit.
function ToolView.new(buf, opts)
function ToolView:set_header(lines)
function ToolView:clear()
function ToolView:append(line)

-- Append without publishing; call flush to render buffered content.
function ToolView:append_buffered(line)
function ToolView:append_text(text)

-- Replace the logical result in one publication. Expansion is view state,
-- so it survives live-result updates while readers never observe a partial card.
function ToolView:replace_lines(lines)
function ToolView:replace_text(text)

-- Append {content} with line numbers, then syntax-highlight it for {ext}
-- asynchronously. Returns false when {content} is empty.
function ToolView:set_highlight(content, ext)
function ToolView:toggle()
function ToolView:flush()
function ToolView:update_line(all_idx, line)

-- Call once after the last append so the collapsed notice renders.
function ToolView:finish()
function ToolView.restore_lines(lines, opts)

-- Rebuild a collapsed view from a tool's saved llm_output, click-to-toggle
-- wired. For `restore` hooks.
function ToolView.restore(output, opts)
```

### `require("n00n.truncate")`

```lua
```

### `require("n00n.usage")`

```lua
function M.normalize(result)
function M.add(total, value)
function M.price(model_spec, result)
```

### `require("n00n.web_backend")`

```lua
--- Select the configured web backend or return a configuration error.
function M.select(requested, firecrawl_configured, fallback, firecrawl_config_error)

--- Remove credentials and control characters before displaying a URL.
function M.sanitize_url(value)

--- Combine provenance and content within strict line and byte limits.
function M.bounded(content, provenance, max_lines, max_bytes)

--- Mark external content as untrusted and identify its backend source.
function M.wrap(content, source)

--- Add fetch provenance while stripping URL credentials from every displayed URL.
function M.fetch(content, backend, requested_url, source_url, final_url)

--- Format credential-safe fetch provenance and content within output limits.
function M.bounded_fetch(content, backend, requested_url, source_url, final_url, max_lines, max_bytes)

--- Mark external content as untrusted while enforcing output limits.
function M.bounded_wrap(content, source, max_lines, max_bytes)

--- Format compact Firecrawl results with untrusted-content provenance.
function M.firecrawl_search(results)

--- Format Firecrawl results within strict line and byte limits.
function M.bounded_firecrawl_search(results, max_lines, max_bytes)
```


{% endraw %}
