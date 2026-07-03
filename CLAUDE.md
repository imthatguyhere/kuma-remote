# kuma-remote

## Dependency Versions

When adding any package, library, framework, tool, plugin, or build dependency, use the latest stable version available from the official package source.

Do not add outdated, deprecated, prerelease, beta, release-candidate, nightly, or unmaintained versions unless the user explicitly requests it or the project requires it for compatibility.

If the latest stable version cannot be used, document the reason in the final response or change notes, including the version chosen and the compatibility constraint.

## Documentation Maintenance

Keep `README.md` and `FUNCTIONALITY.md` synchronized with source changes.

Update `README.md` when a change affects user-facing behavior, prerequisites, configuration, CLI flags, output/logging, architecture, module responsibilities, tracked software, build commands, or release packaging.

Update `FUNCTIONALITY.md` when a change affects `src/` behavior, public/exported types, functions, constants, data flow, algorithms, configuration sources, target software definitions, log table structures, build scripts, release profile settings, or release packaging.

Required `README.md` structure:

- Project title and overview
- Prerequisites
- Building
- Configuration, including all environment variables
- Usage, including all CLI modes and flags
- Output, including stdout behavior, log file path pattern, and report tables
- Architecture and high-level data flow
- Modules
- Tracked Software

Required `FUNCTIONALITY.md` structure:

- Data Flow
- Configuration
- Build and release packaging when non-default build behavior exists
- One H2 section for each source module: `main.rs`, `installed.rs`, `cdk_info.rs`, and `app_logging.rs`
- For each module: purpose, types, constants when applicable, public/exported functions, key internal functions when they explain behavior, and key algorithms
- Target software table showing `installed_name`, `osd_description`, and detection function

When editing Rust source, include documentation updates in the same change. If no documentation update is needed, note why in the final response or change notes.

## Testing

This software is intended to run on other machines, as an installer.
When testing, do not attempt to run the software on the development machine, as it may cause unintended changes to the system.

Instead, do all possible validation prior to running, and any other requested changes from the user, then tell the user what to run/test, if that is blocking progress.
