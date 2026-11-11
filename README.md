# UnifiedOS 👑⚙️


## One Platform to Rule Them All 🏰💻

UnifiedOS is the bold next step in operating systems — running Windows `.exe` and Linux `.deb` apps side-by-side, each inside its own lightweight virtual machine. No more dual boots, no more emulators, just one seamless desktop experience.

---

## 🚀 Why UnifiedOS?

- 👑 **Run any app, anytime** — Windows and Linux apps, unified  
- 🔒 **Strong isolation** — Each app lives safely in its own VM  
- ⚡ **Smooth GUI forwarding** — Apps look native on your desktop  
- 🛠️ **Built for power users and devs** — Freedom without compromise  

---

## 🏗 Architecture

Every application is a VM. Managing different VMs in parallel and presenting
them seamlessly is the goal. A local AI agent operates the system from plain
language, and a hardware planner keeps its model on the GPU first, spilling to
RAM and disk only when the GPU is full.

The design is in [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## ✅ Status

Working today, on a Linux host with KVM:

- `uos run <file>` boots a fresh VM in about a second and runs a Linux
  program, a script, or a Windows console `.exe` (through Wine) inside it. The
  program runs without root, sees only its own folder (read-only unless you
  pass `--writable`), has no network, and its VM is discarded on exit.
- `uos ask "<request>"` hands a plain-language request to a local model that
  inspects and changes the system through tools. Read-only commands run
  directly; everything else asks for approval, and every action is written to
  an audit log.
- `uos hw` shows how the model is split across GPU, RAM and disk.

Not built yet: graphical apps, guest networking, installers (`.deb`, `.msi`),
a system knowledge index, voice input, macOS apps and a bootable image.

## ⚡ Quick start

Requirements: Rust, QEMU with KVM, virtiofsd, podman, and
[Ollama](https://ollama.com) with a tool-calling model.

```bash
cargo build --release
guest/build-image.sh linux      # guest image for Linux apps
guest/build-image.sh windows    # guest image for Windows apps (downloads Wine)

target/release/uos run /path/to/program --its-args
target/release/uos run /path/to/program.exe
target/release/uos ask "which process is using the most memory?"
target/release/uos hw
```

The model defaults to `qwen2.5-coder:32b-instruct` (`ollama pull` it first);
choose another with `--model` or `UOS_MODEL`. Smaller models were unreliable at
calling tools in testing. Guest images carry modules for the running kernel, so rebuild
them after a kernel update.

Tests: `cargo test --workspace`. The end-to-end VM tests need the images:
`cargo test -p uos-vm --test vm_launch -- --ignored`.


## 🔗  Studying 

Explore these projects and technologies to understand how per-app VMs, GUI forwarding, and multi-OS integration work:

- [Qubes OS](https://www.qubes-os.org/) — App isolation with Xen VMs  
- [Firecracker MicroVM](https://firecracker-microvm.github.io/) — Lightweight microVMs  
- [Xpra](https://xpra.org/) — Forward GUI apps remotely or locally  
- [WSLg](https://learn.microsoft.com/en-us/windows/wsl/tutorials/gui-apps) — Linux GUI apps on Windows  

---

## 🤝 Contribute & Collaborate

Ideas, feedback, or contributions are always welcome. Reach out or open issues to discuss.

---

## 📄 License

MIT License © 2025 SAIKUNDANSUDDAPALLI
---

*UnifiedOS — The future of multi-platform computing starts here.*

