# UnifiedOS architecture

UnifiedOS is a layer on top of a Linux host. It does not replace the kernel:
the host kernel provides hardware support and KVM, and UnifiedOS adds three
things on top of it.

1. **Every application runs in its own virtual machine.**
2. **A local AI agent operates the system from natural language.**
3. **A hardware planner keeps the AI model on the GPU first and spills to the
   rest of the machine only when the GPU is full.**

```
        natural language (text now, voice later)
                       |
                 +-----v------+
                 |  uos CLI   |
                 +-----+------+
                       |
        +--------------+---------------+
        |                              |
 +------v-------+              +-------v--------+
 |  uos-agent   |  launch_app  |    uos-vm      |
 | tools, risk  +-------------->  detect, spec, |
 | tiers, audit |              |  launch        |
 +------+-------+              +-------+--------+
        |                              |
 +------v-------+              +-------v--------+
 |   uos-llm    |              |  QEMU/KVM      |
 | Ollama client|              |  one VM per app|
 +------+-------+              +-------+--------+
        |                              |
 +------v-------+              +-------v--------+
 |   uos-hw     |              | guest images   |
 | probe + tier |              | linux, windows |
 | planner      |              +----------------+
 +--------------+
```

## Crates

| Crate       | Responsibility |
|-------------|----------------|
| `uos-hw`    | Reads GPU memory, RAM and CPU count. Plans how a model is split across VRAM, RAM and disk. |
| `uos-llm`   | Talks to the local model server (Ollama). Defines the `ChatBackend` trait so the agent can be tested without a model. |
| `uos-agent` | The agent loop, its tools, the risk classifier, the approval policy and the audit log. |
| `uos-vm`    | Detects what kind of application a file is, builds the VM definition and runs it. |
| `uos-cli`   | The `uos` binary. |

## Per-app virtual machines

`uos run <file>` inspects the file's magic bytes and picks a guest:

| File            | Guest image | How it runs |
|-----------------|-------------|-------------|
| Linux ELF       | `linux`     | Executed directly |
| Script (`#!`)   | `linux`     | Executed directly; the interpreter must exist in the guest |
| Windows PE      | `windows`   | Executed through Wine inside a Linux guest |

Each launch starts a fresh QEMU/KVM machine:

- **Direct kernel boot** of the host's own kernel, with no firmware or
  bootloader stage, so a VM is up in about a second.
- **One shared read-only base image per guest type.** Every VM mounts it with
  `snapshot=on`, so writes go to a throwaway overlay and vanish on exit.
- **Only the app's own folder is shared in**, through virtiofs, at `/app`,
  and read-only unless the user passes `--writable`. The rest of the host
  filesystem is not visible. A program sitting directly in the home directory
  is refused, because its folder would be the whole home directory.
- **The app runs as an unprivileged account** inside the guest, with no way
  to gain privileges.
- **No network device.** Networking is opt-in work for a later milestone.
- The app's arguments travel on the kernel command line. Its input and output
  use the first serial port. Its exit code comes back on a second serial port
  that only the guest's init can open, so an app cannot forge its result.
- A VM that outlives its time limit, or whose caller goes away, is killed.

Running Windows programs through Wine inside a Linux VM keeps the "every app is
a VM" rule without needing a Windows licence. A full Windows guest is the
planned fallback for programs Wine cannot run.

## AI agent

The agent sends the user's request to a local model along with a list of
tools. The model answers with tool calls; the agent runs them and feeds the
results back until the model produces a final answer.

Every tool call is assigned a risk tier before it runs:

| Tier         | Examples | Default |
|--------------|----------|---------|
| `Observe`    | `ls`, `ps`, `df`, reading an ordinary file | Runs without asking |
| `Modify`     | Writing a visible file in the home directory, launching an app in a VM | Asks; `--yes` approves automatically |
| `Privileged` | Every other command, hidden files and credentials, anything outside home | Always asks, `--yes` does not apply |

The model's output is treated as untrusted: it can be steered by the contents
of a file or the output of a command it reads. So the command classifier is an
allow-list, not a block-list. A command is `Observe` only if it is a pipeline
of known read-only programs written with plain characters; any quoting,
expansion, redirection, path to a program, or unknown program makes it
`Privileged`. Read-only commands run with a fixed system `PATH`. Reading or
writing hidden files (`.ssh`, `.bashrc`, `.config`, ...) always asks.

Tool results handed back to the model are capped in size. Every call, approved
or refused, is appended to `~/.local/state/uos/audit.jsonl` before it runs; if
the log cannot be written, the action does not run.

## GPU-first model placement

Before each agent session `uos-hw` reads how much VRAM and RAM is actually
free and works out the model's footprint: its weights plus the attention cache
for the requested context window. VRAM the model server holds for other loaded
models counts as free, because the server unloads them on demand. The layers
are then split across three tiers:

1. **VRAM** – as many layers as fit, up to `--vram-limit` if one is given.
2. **RAM** – the layers that did not fit, run on the CPU.
3. **Disk** – anything beyond free RAM stays memory-mapped and is paged in by
   the kernel on demand.

The resulting layer count is passed to the model server as its GPU offload
setting. If another process has taken GPU memory, fewer layers are offloaded
and the rest of the machine carries them, so the model still loads.

This is the same three-tier idea Colibri uses for mixture-of-experts models,
applied per layer through llama.cpp (which Ollama embeds) rather than through a
new inference engine. A native Rust engine can replace the backend later
behind the `ChatBackend` trait without touching the agent.

## Not built yet

- Graphical apps: forwarding a guest window to the host desktop
  (virtio-gpu with Wayland forwarding).
- Guest networking, with per-app rules.
- A system knowledge index over configs and logs.
- Voice input.
- `.deb`, `.rpm` and `.msi` installers.
- macOS applications.
- A bootable UnifiedOS image.
