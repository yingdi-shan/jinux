<p align="center">
    <img src="docs/src/images/logo_en.svg" alt="asterinas-logo" width="620"><br>
    A safe, fast, and general-purpose OS kernel written in Rust and compatible with Linux
</p>

## Introducing Asterinas

Asterinas is aimed to be a drop-in replacement of Linux but more _memory safe_ and _developer friendly_.

1. Asterinas is more memory safe than Linux as it is written in pure [Rust](https://www.rust-lang.org/) and constraints the scope of unsafe Rust to a well-defined and minimal Trusted Computing Base (TCB). This is what we call the [framekernel]() architecture.
2. Asterinas is more developer friendly than Linux as it allows the kernel developers to use a more productive programming language (Rust), leverage a purpose-built toolkit ([OSDK]()), and enjoy the flexibility of either releasing their kernel modules as open source or keeping them as proprietary (thanks to [MPL](LICENSE-MPL)).

Thanks to its high assurance of memory safety, Asterinas is highly attractive for usage scenarios where Linux ABI is indispensable, but Linux itself is deemed insecure given its sheer size of TCB and its nature of being memory unsafe. An instance of such usage is employing Asterinas as the guest OS for VM TEEs (e.g., [AMD SEV](https://www.amd.com/en/developer/sev.html) and [Intel TDX](https://www.intel.com/content/www/us/en/developer/articles/technical/intel-trust-domain-extensions.html)).

## What's unique about Asterinas

Asterinas is a _zero-cost_, _least-privilege_ OS kernel. A least-privilege OS is one that adheres to the principle of least privileges, which mandates that every unit (e.g., a subsystem, a module, a function, or even an object) of the OS has the minimum authority and permissions required to perform its tasks. This approach reduces the security risk associated with any single element's bugs or flaws. By "zero-cost", we means the same as in Rust's philosophy of zero-cost abstractions, which emphasizes that high-level abstractions (like those that enable least privilege principles) should be available to developers to use, but not come at the cost of performance. Thus, a zero-cost, least-privilege OS enhances its security without compromising its performance.

Some prior OSes do abide by the [principle of least privilege](https://en.wikipedia.org/wiki/Principle_of_least_privilege), but come with considerable performance costs. Take [seL4](https://sel4.systems/) as an example. A [seL4-based multi-server OS](https://docs.sel4.systems/projects/camkes/) minimizes the scope of the OS running in the privileged CPU mode to the seL4 microkernel. The bulk of OS functionalities are implemented by process-based servers, which are separated by process boundary and only allowed to communicate with each other through well-defined interfaces, usually based on RPC. Each service can only access kernel resources represented by [seL4 capabilities](), each of which is restricted with respect to its scope and permissions. All these security measures contribute to the enhanced security of the OS, but result in a considerable runtime costs due to context switching, message passing, and runtime checks.

Asterinas is unique in practicing the principle of least privilege without sacrificing the performance. This is achieved by realizing the full potential of Rust, which, compared to the traditional system programming language like C/C++, offers unique features like an expressive type system, memory safety with unsafe extensions, powerful macros, and a customizable toolchain. By leveraging these unique features, Asterinas is able to construct zero-cost abstractions that enable least privileges at the following three levels.

1. The architectural level. Asterinas is architected as a _framekernel_, where the entire OS resides in a single address space and unsafe Rust code is restricted to a tiny portion of the OS called Asterinas Framework. The Framework exposes safe APIs to the rest of Asterinas, which implements the most of OS functionalities in safe Rust code completely. Thanks to the framekernel architecture, Asterinas's TCB for memory safety is minimized.

![Architecture](docs/src/images/arch_comparison.png)

2. The component level. Upon Asterinas Framework is a set of OS components, each of which is responsible for a particular OS functionality, feature, or device. These OS components are Rust crates with two traits: (1) containing safe Rust code, as demanded by the framekernel architecture, and (2) being governed by Asterinas Component System, which can enforce a fine-grained access control to their public APIs. The access control policy is specified in a configuration file and enforced at compile time, using a static analysis tool.

3. The object level. Asterinas promotes the philosophy of _everything-is-a-capability_, which means all kernel resources, from files to threads, from virtual memory to physical pages, should be accessed through [capabilities](https://en.wikipedia.org/wiki/Capability-based_security). In Asterinas, capabilities are implemented as Rust objects that are constrained in their creation, acquisition, and usage. One common form of capabilities is those with access rights. Wherever possible, access rights are encoded in types (rather than values) so that they can be checked at compile time, eliminating any runtime costs.

As a zero-cost, least-privilege OS, Asterinas provides the best of both worlds: the performance of a monolithic kernel and the security of a microkernel. Like a monolithic kernel, the different parts of Asterinas can communicate with the most efficient means, e.g., function calls and memory sharing. In the same spirit as a microkernel, the fundamental security properties of the OS depend on a minimum amount of code (i.e., Asterinas Framework).

## Build, test and debug Asterinas

While most of the code is written in Rust, the project-scope build process is governed by Makefile. The development environment is managed with Docker. Please ensure Docker is installed and can be run without sudo privilege.

### Preparation
1. Download the latest source code of asterinas.
```bash
git clone [repository url]
```

2. After downloading the source code, run the following command to pull the development image.
```bash
docker pull asterinas/asterinas:0.3.0
```

3. Start the development container.
```bash
docker run -it --privileged --network=host --device=/dev/kvm -v `pwd`:/root/asterinas asterinas/asterinas:0.3.0
```

**All build and test commands should be run inside the development container.**

### Build

1. Build the project.
```bash
make build
```

2. If everything goes well, then we can run the OS.
```bash
make run
```

### Unit Test

#### User mode unit tests

Many of our crates does not require running on bare metal environment and can be tested through the standard Cargo testing framework. A specific list of which crates can be tested with `cargo test` is listed in the `Makefile` of the root workspace, and by using Make you can test all these crates together.

```bash
make test
```

Nevertheless, you could enter the directory of a specific crate and invoke `cargo test` to perform user mode unit tests and doctests.

#### Kernel mode unit tests

We can run unit tests in kernel mode for crates like `aster-frame` or `aster-std`. This is powered by our [ktest](framework/libs/ktest) framework.
```bash
make run KTEST=1
```

You could also specify tests in a crate or a subset of tests to run.
```bash
make run KTEST=1 KTEST_WHITELIST=failing_assertion,aster_frame::test::expect_panic KTEST_CRATES=aster-frame
```

#### Component check

If we want to check access control policy among components, install some standalone tools (e.g., `cargo-component`).
``` bash
make tools
```

Then we can use the tool to check access control policy.
```bash
cargo component-check
```

### Integration Test

#### Regression Test

This command will automatically run Asterinas with the test binaries in `regression/apps` directory.
```bash
make run AUTO_TEST=regression
```

#### Syscall Test

This command will build the syscall test binary and automatically run Asterinas with the tests using QEMU.
```bash
make run AUTO_TEST=syscall
```

Alternatively, if you wish to test it interactively inside a shell in Asterinas.
```bash
make run BUILD_SYSCALL_TEST=1
```

Then, we can run the following script using the Asterinas shell to run all syscall test cases.
```bash
/opt/syscall_test/run_syscall_test.sh
```

### Debug

To debug Asterinas using [QEMU GDB remote debugging](https://qemu-project.gitlab.io/qemu/system/gdb.html), you could compile Asterinas in debug mode, start a Asterinas instance and run the GDB interactive shell in another terminal.

To start a QEMU Asterinas VM and wait for debugging connection:
```bash
make run GDB_SERVER=1 ENABLE_KVM=0
```

To get the GDB interactive shell:
```bash
make run GDB_CLIENT=1
```

Currently, the Asterinas runner's debugging interface is exposed by unix socket. Thus there shouldn't be multiple debugging instances in the same container. To add debug symbols for the underlying infrastructures such as UEFI firmware or bootloader, please check the runner's source code for details.

## Code organization

The codebase of Asterinas is organized as below.

* `runner/`: creating a bootable Asterinas kernel image along with an initramfs image. It also supports `cargo run` since it is the only package with `main()`.
* `kernel/`: defining the entry point of the Asterinas kernel.
* `framework/`: the privileged half of Asterinas (allowed to use `unsafe` keyword)
    * `aster-frame`: providing the safe Rust abstractions for low-level resources like CPU, memory, interrupts, etc;
    * `libs`: Privileged libraries.
* `services/`: the unprivileged half of Asterinas (not allowed to use `unsafe` directly), implementing most of the OS functionalities.
    * `comps/`: Asterinas OS components;
    * `libs/`: Asterinas OS libraries;
        * `aster-std`: this is where system calls are implemented. Currently, this crate is too big. It will eventually be decomposed into smaller crates.
* `tests/`: providing integration tests written in Rust.
* `regression/`: providing user-space tests written in C.
* `docs/`: The Asterinas book (needs a major update).

## Development status

Asterinas is under active development. The list below summarizes the progress of important planned features.

* Technical novelty
    - [X] Asterinas Framework
    - [X] Asterinas Component System
    - [X] Asterinas Capabilities
* High-level stuff
    * Process management
        - [X] Essential system calls (fork, execve, etc.)
        * Supported file formats:
            - [X] ELF
            - [X] Scripts (Shebang)
        - [X] Process group
        - [ ] Process credentials
    * Thread management
        - [X] Essential system calls (clone, futex, etc.)
    * Scheduling
        - [X] Essential system calls (yield, nanosleep, etc.)
        * Sleeping
        * Schedulers
            - [ ] Scheduler framework
            - [X] Round-robin scheduler
        - [ ] Preemption
    * Memory management
        - [X] Essential system calls (mmap, munmap, etc.)
        * Mapping types
            - [X] Anonymous
            - [X] File-backed
        * Optimizations
            - [X] On-demand paging
            - [X] Copy-on-write
    * File system
        * Essential system calls 
            - [X] File I/O (read, write, etc.)
            - [X] FS operations (create, unlink, etc.)
        * VFS
            - [X] The VFS framework
            - [X] Dentry cache
            - [X] Page cache
        * File systems
            - [X] Ramfs
            - [X] Initramfs
            - [X] Procfs
            - [ ] Ext2/3/4
    * Inter-process communication
        * POSIX signals
            - [X] Essential system calls (sigaction, sigreturn, kill, etc.)
        - [X] Pipe
    * Network I/O
        * I/O multiplexing
            - [X] poll
            - [X] select
            - [X] epoll
        * Socket types
            - [X] TCP sockets
            - [X] Unix sockets
            - [ ] UDP sockets
    * Time and clocks
* Low-level stuff
    * Devices
        - [X] PCI
        - [X] System time 
        - [X] Tty
        - [X] Virtio-blk
        - [X] Virtio-net
        - [ ] Device abstraction layer
    * CPU archs
        - [X] x86-64
            - [ ] AMD SEV
            - [ ] Intel TDX
        - [ ] Arm
        - [ ] RISC-V
    * Misc
        - [ ] Multi-core support
        - [ ] User-mode development

## License

The Asterinas project is released under the license of [Mozilla Public License Version 2.0](LICENSE-MPL).
See [COPYRIGHT](COPYRIGHT) for details.
