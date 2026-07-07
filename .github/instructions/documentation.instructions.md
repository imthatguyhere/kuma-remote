---
description: always
applyTo: "*"
---

Provide project context and coding guidelines that AI should follow when generating code, answering questions, or reviewing changes.

# Documentation Maintenance Instructions

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
- One H2 section for each source module.
  - For each module: purpose, types, constants when applicable, public/exported functions, key internal functions when they explain behavior, and key algorithms
- Checks table.

When editing Rust source, include documentation updates in the same change. If no documentation update is needed, note why in the final response or change notes.
