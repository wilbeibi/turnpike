<h1 align="center">turnpike</h1>

<h3 align="center">See which of your tools is spending your LLM API money</h3>

<p align="center">
  <img src="assets/turnpike-banner.png" alt="turnpike watches streams of LLM API usage and prints a cost receipt" width="720">
</p>

turnpike is a local LLM API cost tracker: a small proxy that sits between your tools
and OpenAI, Anthropic, and friends, and writes down what every call actually cost.

Here's the situation it's for. Your API bill is bigger than you expected. You have a
coding agent, a couple of chat CLIs, and some script you wrote at 1am that summarizes
web pages — all on the same keys. One of them is the problem. You have no idea which.

So: point a tool at turnpike instead of the provider, keep the same API key, and it
writes down every call — model, tokens, cost, latency, and which tool made it. Then
you read it back from the terminal. That's the whole thing.

It only watches. Your request goes upstream as you sent it — headers, key, body —
with one exception: on streaming calls to OpenAI-shaped providers it sets
`stream_options.include_usage`, which is the only reason the provider tells anyone
the token counts. Everything else happens off to the side. Nothing leaves your
machine, and if the recording breaks, your request still goes through.

Questions it answers:

- Which tool spent the most today, and which model ate the most tokens.
- Whether that failed request still reached the provider and billed you anyway.
- Whether your cache hits are actually landing.
- Which local tools are quietly calling which APIs.

Works with **OpenAI**, **Anthropic**, **Gemini**, **DeepSeek**, **OpenRouter**,
**Kimi**, **MiniMax**, **GLM**, **xAI**, and **Groq**.

## What it looks like

```text
$ turnpike tail -n 4

[2026-07-19T10:42:18Z] anthropic claude-sonnet-4-5 200 1243ms tokens=16200→2450 cache_read=45200 $0.1132 client=opencode/0.4
[2026-07-19T10:50:02Z] anthropic claude-sonnet-4-5 429 820ms tokens=? $0.0000 ERROR=rate_limit client=opencode/0.4
[2026-07-19T10:43:22Z] openai gpt-4.1-mini 200 312ms tokens=1205→88 cache_read=980 $0.0003 client=hermes/1.2
[2026-07-19T10:45:11Z] openrouter qwen/qwen3-coder-480b 200 2100ms tokens=44021→12134 $0.0700 client=opencode/0.4
```

```text
$ turnpike stats --by-model

model                  calls  input    output   cache_read  cache_write  cache%  errors  p50_ms  p95_ms  cost_usd
---------------------  -----  -------  -------  ----------  -----------  ------  ------  ------  ------  ----------
claude-sonnet-4-5      3      24300    3650     57400       3800         67%     1       980     1243    0.1591
gpt-4.1-mini           2      2047     207      980         0            48%     0       312     640     0.0009
qwen/qwen3-coder-480b  1      44021    12134    0           0            -       0       2100    2100    0.0700
```

## Install

Every [release](https://github.com/wilbeibi/turnpike/releases) has a binary for
Linux (x86_64, aarch64) and macOS (Apple silicon, Intel). Unpack one onto your
`PATH`, or let `cargo binstall` fetch the right one:

```zsh
cargo binstall turnpike        # prebuilt binary
cargo install turnpike         # or build it yourself, if you have a toolchain
```

```zsh
turnpike start                 # start the listeners (runs in the foreground)
turnpike prices pull           # optional: pull a price table so costs are filled in
```

The Linux builds want glibc 2.35 or newer (Ubuntu 22.04+, Debian 12+, Arch).
There is no Windows build — the crate doesn't compile there yet.

Or from a clone, to build what's on `main`:

```zsh
git clone https://github.com/wilbeibi/turnpike
cd turnpike
cargo install --path .
```

### Keeping it running

`turnpike start` stays in the foreground and has no daemon mode. Your tools can only
reach it while it's up, so give it to a supervisor and forget about it. On Linux,
a user unit at `~/.config/systemd/user/turnpike.service`:

```ini
[Service]
ExecStart=%h/.cargo/bin/turnpike start
Restart=always

[Install]
WantedBy=default.target
```

```zsh
systemctl --user enable --now turnpike
```

`ExecStart` has to be the binary's real path — `~/.cargo/bin` if you installed
with cargo, wherever you unpacked it if you took a release tarball.

On macOS, a launchd agent or any process manager works the same way; a terminal
tab or `tmux` window is fine while you're trying it out.

## Usage

Point a tool at turnpike and use it exactly as before. `turnpike config` is where
you find out what to point it at:

```zsh
turnpike config                           # every provider, and what this shell does with it
turnpike config deepseek                  # http://127.0.0.1:4003/v1 — paste that into an app
eval $(turnpike config deepseek --shell)  # or point this shell at it, in $SHELL's own syntax
```

Eight of the ten providers read `OPENAI_BASE_URL`, so only one of them can be routed
by environment at a time — which is why an export needs a name, and why the bare
table is worth a look. It says which one currently wins, and which keys are sitting
here with nothing pointing at turnpike:

```
provider    base URL                  this shell
----------  ------------------------  ----------
openai      http://127.0.0.1:4000/v1  -
deepseek    http://127.0.0.1:4003/v1  routed
xai         http://127.0.0.1:4008/v1  direct
```

`routed` means a variable in this shell points at turnpike. `direct` means a key is
set with nothing pointing at turnpike, so anything that takes its base URL from the
environment will spend it unmetered. `-` means no key found. It's the environment
turnpike was launched from, not a claim about the whole machine — a tool that sets
its own base URL in code is invisible from out here, which is also why Gemini reads
`in code` rather than a guess.

Scripts and coding agents want `turnpike config --format json`: the same picture,
plus each provider's port, path, key variables and upstream. Either beats copying a
URL out of this README.

Then read back what you used:

```zsh
turnpike tail -n 10 --since 2h
turnpike stats --since 7d
turnpike stats --by-model
turnpike stats --by-client     # which declared client spent it
turnpike stats --by-tool       # best identity per call: declared header,
                              #   else observed process (peer_exe), else User-Agent
turnpike stats --by-day        # daily trend
```

`--by-tool` groups each call under its single best tool identity — the declared
`x-turnpike-client` header when the caller sends one, otherwise the observed calling
process (`peer_exe`), falling back to the `User-Agent`. Old rows (before `client_source`
existed) keep the historical client-then-process order.

Add `--json` to `stats` or `tail` for machine-readable output.

### Budget check

Pick a number you don't want to cross, and ask. `turnpike check` prints a one-line
answer, and — the actually useful part — sets its exit code: `0` under, `1` at or
over, `2` on error (bad `--budget`, corrupt data — fix the invocation or go
investigate), `3` unknown (nothing's broken, turnpike just can't vouch for the
number yet — no calls recorded, or some have no price):

```zsh
turnpike check --budget 50/day
# day: $14.20 / $50.00 (28%) — ok

turnpike check --budget 50/day --json
# {"window":"day","spent":14.2,"budget":50.0,"over":false,"remaining":35.8, ...}
```

The period is `day`, `week`, or `month` (calendar windows in your local time, so
`month` lines up with a billing cycle), or any `--since` form for a rolling
window (`300/7d`, `20/24h`). turnpike won't send the alert for you — it's a meter,
not a notifier — so hook it up to whatever already yells at you:

```zsh
# quiet, exit-code only: four outcomes, not two
turnpike check --budget 50/day -q
status=$?
case $status in
  0) ;;                              # under budget
  1) ntfy send "LLM budget blown" ;; # at/over — the actionable alert
  3) ;;                              # unknown — no data yet, or a price gap; not an error
  *) exit $status ;;                 # 2: something's actually broken — propagate, don't swallow
esac

# a shell prompt segment, a cron/timer line, or a coding-agent hook can all
# just run `turnpike check` and branch on the exit code
```

### Providers and ports

`turnpike config <name>` prints the base URL, so you rarely type one by hand. When
you do, you can address turnpike **by name**:

```
http://<provider>.localhost:4000<path>     # e.g. http://openai.localhost:4000/v1
```

One port for every provider, and the name is just the provider's own — nothing to
look up. A name turnpike doesn't know is refused outright, so a typo can never be
sent to the wrong provider with the wrong key. turnpike answers on both IPv4 and
IPv6 loopback, so the name works whether the client reaches for `127.0.0.1` or `::1`.

The `<path>` suffix mirrors each vendor's own API, so your SDK's base URL lines up
with the endpoint it will call — that part is the vendor's, not turnpike's.

Each provider also listens on its own fixed port. That's the form `turnpike config`
prints, because it's the one that resolves everywhere: browsers and Linux handle
`*.localhost`, macOS and slim containers sometimes don't. It's
`http://127.0.0.1:<port><path>`:

| Provider | Port | Path | Upstream |
| --- | --- | --- | --- |
| OpenAI | 4000 | `/v1` | `https://api.openai.com` |
| Anthropic | 4001 | — | `https://api.anthropic.com` |
| Gemini | 4002 | — | `https://generativelanguage.googleapis.com` |
| DeepSeek | 4003 | `/v1` | `https://api.deepseek.com` |
| OpenRouter | 4004 | `/api/v1` | `https://openrouter.ai` |
| Kimi | 4005 | `/v1` | `https://api.moonshot.ai` |
| MiniMax | 4006 | `/v1` | `https://api.minimaxi.com` |
| GLM | 4007 | `/api/paas/v4` | `https://open.bigmodel.cn` |
| xAI | 4008 | `/v1` | `https://api.x.ai` |
| Groq | 4009 | `/openai/v1` | `https://api.groq.com` |

## Cost

Costs come from what the provider reports. When a provider doesn't report one,
turnpike works it out from a local price table you can refresh:

```zsh
turnpike prices pull    # refresh the table from models.dev
turnpike prices show    # what the models you actually call cost right now
```

`prices show` reads the table against your call log, so it lists the handful of
models you use rather than the couple of thousand it knows about — the rate in
force this minute, whether a time-of-day discount is currently applying, and a
loud `NO PRICE` on anything you call that the table can't price. Models whose
provider reports its own cost say so: for those the table is never consulted.

Every call is priced at the rates in force **when it was made**, not at today's
rates. When an upstream price moves, `prices pull` appends a dated revision
rather than replacing what's there, so old rows keep the price you actually
paid and a provider's increase doesn't retroactively rewrite last month. It
stamps the revision with the pull time — if you know the real date, edit
`effective_from` in `prices.json` and the next pull will leave it alone.

That makes `prices.json` the only record of your price history, so it's treated
as your data rather than a cache: a pull keeps the previous copy at
`prices.json.bak` and swaps the new one in atomically, and a table it can't
parse is an error that leaves the file untouched — never a reason to rebuild it
from scratch.

Providers that bill by the clock get recurring discount windows. DeepSeek,
whose off-peak rate is half its peak rate and whose peak hours are weekday
business hours at home:

```jsonc
"deepseek-v4-pro": {
  "input_per_m": 0.435, "output_per_m": 0.87, "cache_read_per_m": 0.003625,
  "cache_in_input": true,
  "pinned": true,            // upstream lists one of the two rates; don't clobber this
  "revisions": [{
    "effective_from": "2026-08-22T16:00:00Z",
    "input_per_m": 1.32, "output_per_m": 3.96, "cache_read_per_m": 0.044,
    "off_peak": { "multiplier": 0.5, "tz": "+08:00", "windows": [
      "Mon-Fri 00:00-09:00", "Mon-Fri 12:00-14:00", "Mon-Fri 18:00-24:00",
      "Sat-Sun 00:00-24:00"
    ]}
  }]
}
```

`tz` is the offset the windows are written in, so you can transcribe a schedule
the way its provider publishes it instead of translating it into UTC first —
here the four windows are visibly the week with two holes in it, 09:00–12:00
and 14:00–18:00, which is what DeepSeek charges peak for. Leave `tz` out and
the windows are UTC. Fixed offsets only: no provider prices against a clock
that shifts twice a year.

A window with no day in front of it recurs every day, the way they all used to.
`Mon-Fri`, `Sat-Sun`, and `Mon,Thu` restrict it, and a window may wrap midnight
into the next day — `Sun 16:00-01:00` runs to Monday morning.

Base rates always hold the undiscounted price, so a window you forget to write
overcharges you on paper rather than hiding spend.

A revision only has to state what changed. Anything it leaves out — cache
accounting, context tiers — it inherits from the base entry, so a hand-written
revision can't quietly delete pricing it didn't mention. To record a tier a
provider actually retired, say so with `"tiers": []`.

turnpike never guesses tokens a provider didn't report. A call that comes back with
no usage is saved as exactly that — no counts, marked `no_usage` — so nothing
quietly reads as free.

## What it records, and what it doesn't

One row per call, usage only: time, provider, model, endpoint, status, latency,
token counts, cost, and the tool that made the call.

**Your prompts and responses are never stored.** Keys or credentials that show up
in error text are scrubbed before anything is written. It all lives in a local
SQLite file you own:

```text
${XDG_DATA_HOME:-$HOME/.local/share}/turnpike/calls.db
```

Read it with `stats` and `tail`, or open it with any SQLite tool. (The stored
`cost` column holds only costs the provider itself reported; `stats` and `tail`
add the computed ones, so use those for a full total.)

## Coding agents

Agents are half the point — they write the scripts that spend the money, and they're
the ones that will happily hardcode `api.openai.com`. Two skills under `skills/` teach
them the grammar: `turnpike`, which covers pointing a client at the proxy, naming it
with `x-turnpike-client`, and reading the cost back, and `turnpike-spend-review`, a
full teardown of where the money went and whether a cheaper model would do.

Install by pointing your agent's skill directory at them:

```zsh
ln -s "$PWD/skills/turnpike" "$PWD/skills/turnpike-spend-review" ~/.claude/skills/
```

## Alternatives

There are bigger tools in this space. Most of them do more than you need for
"which of my laptop's tools is burning money":

| | What it is | Why you might not want it here |
| --- | --- | --- |
| **LiteLLM / gateways** | routing, load balancing, key vaults, fallbacks | holds your keys, rewrites requests, one endpoint for everything — a mistyped model can land on the wrong provider |
| **Helicone, Langfuse, Braintrust** | hosted LLM observability, traces, prompt history | your prompts leave the machine, and you get an account, a dashboard, and a bill |
| **Provider dashboards** | the numbers the vendor already has | one per provider, a day late, and no idea which local tool made the call |
| **turnpike** | a local meter, one SQLite file | doesn't route, cache, retry, or hold keys — if you want a gateway, this isn't one |

The short version: if you want a control plane, use LiteLLM. If you want a trace
viewer for a production app, use Langfuse. If you want to know which thing on your
laptop spent $40 last Tuesday, that's this.

## Boundaries

- A meter, not a gateway. It doesn't route, balance, cache, retry, hold budgets,
  or store your keys.
- Local only. Listeners bind loopback (`127.0.0.1` and `::1`), never `0.0.0.0`;
  nothing is network-exposed.
- Usage metadata only. No prompt or response bodies, ever.
- No external telemetry. Just the local file.

## Status

`0.3.0`. One Rust binary, MIT-licensed.
