# A GUI for Cargo

This is a project to make a GUI for cargo

This project is made as a showcase for

## The idea

```sh
cargo install cargo-ui
cargo ui
```

Double-click on a `Cargo.toml` file would also run cargo-ui.

## Vision

Some idea of feature

 - [x] choose the binary to run or the lib to build or the test to run
 - [x] Display the errors in a nice way
 - [x] select the debug or release mode
 - [ ] select the toolchain (nightly, stable, ...)
 - [ ] maybe integrate with rustup to update the toolchain or  install new one
 - [ ] See the dependencies as an expendable tree
 - [ ] Show duplicated dependencies
 - [ ] Show outdated dependencies, with button to easily update
 - [ ] Ability to easily add dependency (by searching the crates.io index)
 - [ ] edit features of dependencies from a list.
 - [ ] show asm, llvm-ir, ...
 - [ ] show build progress and be able to cancel the build
 - [ ] also edit other metadata of the the Cargo.toml (edition, author, ...)
 - [ ] manage workspace and do batch edit of the metadata on all members
 - [ ] ...
