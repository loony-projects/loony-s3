# Documentation

- **[usage.md](usage.md)** — start here. Build, run standalone, talk to it (bundled
  scripts, rclone, curl), run the web UI.
- **[configuration.md](configuration.md)** — every environment variable and CLI flag,
  what's required, what it defaults to.
- **[api-reference.md](api-reference.md)** — the exact LS3 API surface: supported
  operations, auth, naming rules, durability behavior, error codes. What's implemented
  and what genuinely isn't yet.
- **[cluster.md](cluster.md)** — running more than one node, and a precise statement of
  what's actually distributed today versus what isn't yet.
- **[architecture.md](architecture.md)** — the design document: *why* it's built this
  way, the full crate-dependency diagram, and the phase-by-phase implementation plan.
  Read this before writing or reviewing code in `crates/` — it's the baseline every
  phase stays consistent with.
