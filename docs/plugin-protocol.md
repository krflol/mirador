# External panel protocol v1

Mirador can adapt an explicitly configured child process to its private
`Panel` trait. The boundary is deliberately a process protocol, not a Rust
library API:

- the release binary embeds no interpreter and loads no dynamic library;
- no directory, entry point, package registry or executable is scanned;
- a declaration starts nothing unless its id is placed in the layout;
- a broken or blocked plugin cannot block Mirador's event or render thread;
- the child never writes to the real terminal. It publishes state and Mirador
  remains the sole renderer.

A plugin is arbitrary code with the user's permissions. The protocol is crash
isolation, not a security sandbox. Only configure commands you trust.

## Configuration and lifecycle

Commands are argv arrays and are launched directly, without a platform shell.
That gives paths and quoting the same meaning on Windows, Linux and macOS.

```toml
[[plugins]]
id = "example"
command = ["example-mirador-plugin", "--optional-argument"]

[plugins.config]
answer = 42

[layout]
rows = [
  { height = 100, panels = [{ widget = "example", width = 100 }] },
]
```

The id is a lowercase ASCII letter followed by lowercase letters, digits,
hyphens or underscores. It cannot duplicate another plugin or a built-in
widget id. `plugins.config` is plugin-owned TOML and reaches the child as JSON;
Mirador does not interpret its schema.

Each placement owns one child process. Mirador pipes all three standard
streams. Stdin and stdout carry UTF-8 JSON Lines; stderr is a bounded diagnostic
tail shown on failure. On normal removal or exit, Mirador sends `shutdown`,
allows 300 ms for cleanup, then terminates a child that remains. A failed panel
can be restarted with `r`.

## Framing and negotiation

Each message is one JSON object followed by `\n`, at most 8 MiB including its
content. Unknown or malformed messages end the plugin session rather than
guessing at a newer contract.

The first host message is:

```json
{
  "type": "hello",
  "protocol": 1,
  "host_version": "1.6.0",
  "plugin": "example",
  "config": {"answer": 42},
  "cwd": "/current/working/directory"
}
```

The first child message must be:

```json
{
  "type": "ready",
  "protocol": 1,
  "title": "Example",
  "refresh_ms": 100
}
```

The protocol version must match exactly. `refresh_ms` is clamped to 16 through
60,000 ms. A ready message is sent once.

## Child-to-host messages

Frames are complete immutable snapshots, not patches. Revisions must increase;
late or duplicated revisions are ignored. The reader thread keeps only the
newest snapshot, so a producer cannot create an unbounded frame queue.

```json
{
  "type": "frame",
  "revision": 7,
  "title": "Example",
  "counter": "ready",
  "lines": [
    {"spans": [
      {"text": "hello ", "fg": "theme:text"},
      {"text": "world", "fg": "ansi:10", "bold": true}
    ]}
  ],
  "bindings": [
    {"key": "Enter", "action": "open", "primary": true}
  ],
  "input": {
    "capture": false,
    "keys": ["Enter"],
    "interrupt": false,
    "paste": false,
    "mouse": false
  },
  "cursor": {"column": 3, "row": 0, "visible": true}
}
```

Every span requires `text`. Optional `fg` and `bg` values accept:

- `default` or `reset`;
- `theme:border`, `theme:border_focused`, `theme:rule`, `theme:title`,
  `theme:text`, `theme:muted`, `theme:label`, `theme:accent`, `theme:key`,
  `theme:success`, `theme:warning`, `theme:error` or `theme:track`;
- `ansi:0` through `ansi:255`;
- a ratatui colour name or `#rrggbb`.

The optional style flags are `bold`, `dim`, `italic`, `underlined` and
`reversed`. Coordinates are zero-based and relative to the panel interior.

A child may also report a visible non-fatal or fatal error:

```json
{"type":"error","message":"connection lost","fatal":false}
```

A fatal error ends the process. A non-fatal error remains a panel notice until
a later state replaces it.

## Host-to-child messages

After negotiation the host may send:

```json
{"type":"resize","columns":80,"rows":24}
{"type":"focus","focused":true}
{"type":"tick"}
{"type":"interrupt"}
{"type":"paste","text":"one\ntwo"}
{"type":"shutdown"}
```

Keys include both a stable display-independent code and the canonical chord
used by `input.keys`:

```json
{
  "type":"key",
  "key":"Ctrl+x",
  "code":"char",
  "text":"x",
  "modifiers":["Ctrl"]
}
```

Named canonical keys include `Enter`, `Backspace`, `Tab`, `BackTab`, `Esc`,
`Left`, `Right`, `Up`, `Down`, `Home`, `End`, `PageUp`, `PageDown`, `Delete`,
`Insert`, and `F1` through `F12`. Modifier order is `Ctrl`, `Alt`, `Shift`,
`Super`, `Hyper`, `Meta`. Character case is preserved.

Mouse coordinates are panel-relative:

```json
{
  "type":"mouse",
  "kind":"down",
  "button":"left",
  "column":4,
  "row":2,
  "modifiers":[]
}
```

Kinds are `down`, `up`, `drag`, `move`, `scroll_up`, `scroll_down`,
`scroll_left`, and `scroll_right`; buttons are `left`, `middle`, or `right`.

## Input ownership

Input policy is evaluated synchronously from the newest frame. Mirador never
waits for a child to decide whether a global key is safe.

- `capture` consumes all focused keys.
- `keys` consumes only those canonical chords while otherwise passive.
- `paste` and `mouse` opt into those event classes.
- `interrupt` may consume one Ctrl+C. Mirador disarms it locally before
  enqueueing the event; another non-interrupt input rearms it. Therefore
  Ctrl+C twice always exits even if the plugin process is stuck.

After Mirador accepts a named key from a passive panel, it conservatively
captures subsequent input until a newer frame acknowledges the action. This
closes the asynchronous transition where, for example, `Enter` engages a
terminal but a rapidly typed `q` arrives before the terminal's `capture` frame.
Plugins should publish a new frame after handling any named passive key.

If a capturing plugin stops reading and its bounded input queue fills, events
are dropped and the panel reports the stall, but they never fall through as
Mirador global actions. A pasted `q` or queued `q` cannot become a quit.
