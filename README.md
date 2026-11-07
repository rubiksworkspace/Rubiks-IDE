> [!IMPORTANT]
> Remove this line to confirm you've reviewed this PR before submitting.
# Rubiks IDE

Rubiks IDE is a code editor for [Rubiks Workspace](https://rubiksworkspace.com), integrated with [Prime Agent (Rubiks)](https://github.com/rubiksworkspace/rubiks-harness). It is a modified version of [Zed](https://github.com/zed-industries/zed), the high-performance, multiplayer code editor from Zed Industries.

Rubiks IDE is an independent project. It is not made, endorsed or supported by Zed Industries, so please report problems [here](https://github.com/rubiksworkspace/Rubiks-IDE/issues), not to the Zed project.

---

### Installation

Rubiks IDE is published for Apple silicon Macs.

1. Download [Rubiks-IDE-aarch64.dmg](https://github.com/rubiksworkspace/Rubiks-IDE/releases/latest/download/Rubiks-IDE-aarch64.dmg) from the [latest release](https://github.com/rubiksworkspace/Rubiks-IDE/releases/latest).
2. Open it and drag Rubiks IDE to Applications.
3. The build is not notarized, so on first launch right-click Rubiks IDE and choose Open, or run:

   ```bash
   xattr -dr com.apple.quarantine "/Applications/Rubiks IDE.app"
   ```

Once installed, the IDE checks for a new release every hour and updates itself. When you are signed in and your company setup includes Prime Agent, each launch also installs or updates Prime Agent (Rubiks).

### What differs from Zed

- Rubiks Workspace sign-in, with company settings applied by role.
- Prime Agent (Rubiks), installed and kept up to date by the IDE.
- A Prime Agent Data tab for browsing saved sessions, research logs and harness memory, plus session surveys.
- Updates come from this repository's GitHub releases instead of Zed's servers.
- Rubiks branding: name, icons and themes.

[NOTICE](./NOTICE) records the origin of this code and of these changes.

### Developing Rubiks IDE

- [Building for macOS](./docs/src/development/macos.md)
- [Building for Linux](./docs/src/development/linux.md)
- [Building for Windows](./docs/src/development/windows.md)

These guides, and much of the documentation in `docs/`, come from Zed and still use its name.

### Contributing

[CONTRIBUTING.md](./CONTRIBUTING.md) is inherited from Zed. Its code guidance applies here; send issues and pull requests to this repository.

### Licensing

Rubiks IDE is licensed primarily under [GPL-3.0-or-later](./LICENSE-GPL), with [Apache-2.0](./LICENSE-APACHE) components where marked, the same terms as the Zed source it is based on. Each crate's `Cargo.toml` states its license.

License information for third party dependencies must be correctly provided for CI to pass.

We use [`cargo-about`](https://github.com/EmbarkStudios/cargo-about) to automatically comply with open source licenses. If CI is failing, check the following:

- Is it showing a `no license specified` error for a crate you've created? If so, add `publish = false` under `[package]` in your crate's Cargo.toml.
- Is the error `failed to satisfy license requirements` for a dependency? If so, first determine what license the project has and whether this system is sufficient to comply with this license's requirements. If you're unsure, ask a lawyer. Once you've verified that this system is acceptable add the license's SPDX identifier to the `accepted` array in `script/licenses/zed-licenses.toml`.
- Is `cargo-about` unable to find the license for a dependency? If so, add a clarification field at the end of `script/licenses/zed-licenses.toml`, as specified in the [cargo-about book](https://embarkstudios.github.io/cargo-about/cli/generate/config.html#crate-configuration).

### Credits

Rubiks IDE is built on the work of Zed Industries, Inc. and the Zed contributors. "Zed" is used here only to say where this code comes from.
