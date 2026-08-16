Here is the `DESIGN.md` for **Oxid**. This document establishes the visual identity, UI/UX language, and aesthetic rules across all touchpoints (CLI, TUI, Web, and Desktop).

It borrows heavily from the Rust programming language's aesthetic: dark, industrial, reliable, with striking typography and the iconic "Rust" orange.

---

# DESIGN.md: The Visual Identity of Oxid

> **Design Philosophy: "Industrial Elegance."**
> Oxid is a tool built by engineers, for engineers. It should feel like a heavy-duty anvil: indestructible, cold, and precisely engineered. The interface defaults to dark mode, relying on high-contrast accents and structural typography. It does not use soft shadows or rounded bubbles; it uses hard borders, mono-spaced data, and glowing indicators.

## 1. Color Palette

The color scheme is directly inspired by forging metal, oxidation processes, and terminal environments.

| Color Name       | Hex Code  | Usage                                                        | Meaning / Association                                                             |
| ---------------- | --------- | ------------------------------------------------------------ | --------------------------------------------------------------------------------- |
| **Oxid Orange**  | `#DE5236` | Primary buttons, active CLI flags, active branch highlights. | The core Rust color. Represents action, heat, and active deployments.             |
| **Carbon Black** | `#121212` | App backgrounds, main CLI background.                        | The void. Minimal energy consumption.                                             |
| **Iron Gray**    | `#262626` | Card backgrounds, TUI panels, secondary UI elements.         | Sturdy infrastructure.                                                            |
| **Steel White**  | `#F4F4F5` | Primary text, headings, icons.                               | Clear, readable, uncompromised data.                                              |
| **Patina Green** | `#4A9E79` | Success states, `Running` status.                            | Oxidized copper turns green. Represents successful builds and healthy containers. |
| **Ash Gray**     | `#6B7280` | Muted text, `Paused` / `Scale-to-Zero` status.               | Dormant processes, hibernating containers.                                        |

## 2. Typography

We adopt the official fonts of the Rust ecosystem to maintain deep visual cohesion with the language it was built in.

- **Primary Font (UI & Headings):** `Fira Sans`
- _Why:_ It’s the font used on the official rust-lang.org website. It provides a clean, highly legible humanist sans-serif structure that feels modern but grounded.
- _Usage:_ Web dashboard headings, Desktop app navigation, buttons.

- **Monospace Font (Code & CLI):** `Fira Code` (with ligatures enabled)
- _Why:_ Designed specifically for programming. The ligatures (`=>`, `->`, `!=`) make terminal output and configuration files (`oxid.toml`) beautiful and easy to parse.
- _Usage:_ CLI output, TUI interface, log streaming, branch names, and commit hashes.

## 3. UI/UX Principles across Platforms

### 3.1. State Visualization (The "Scale-to-Zero" Look)

Because Oxid's core feature is putting environments to sleep, the UI must clearly communicate these states without the user having to read text labels:

- **Running (Active):** Branch name in **Steel White** with a pulsing **Patina Green** indicator.
- **Paused (Scale-to-Zero):** Entire row/card dims to **Ash Gray**. The text becomes italicized. Clicking it or sending traffic instantly restores it to full color.
- **Building:** A spinning loader in **Oxid Orange**.

### 3.2. Web Dashboard & Desktop App (Tauri)

- **Borders & Shapes:**
- Use sharp corners (`border-radius: 2px` or `0px`).
- Use hard 1px solid borders (`#333333`) instead of drop shadows to separate cards.

- **Layout:** Brutalist and data-dense. Maximize horizontal space for log streams.
- **Interactivity:** Hovering over a paused branch should show a tooltip in Fira Code: `<Click environment to wake>`.

### 3.3. Command Line Interface (CLI)

The CLI must feel incredibly fast. It should use standard ANSI escape codes mapping to our palette.

- **Prefixes:** Every Oxid output line should be prefixed for readability.
- `[+]` in **Patina Green** for success.
- `[~]` in **Ash Gray** for background tasks (e.g., pausing).
- `[>]` in **Oxid Orange** for actionable prompts or active builds.

- **Example Output:**

```text
[>] oxid up feature-login
[+] Parsed oxid.toml successfully
[+] Shared Postgres instance detected. Created db_feature_login
[>] Building image (Cache hit: 85%) ...
[+] Environment live at: https://feature-login.local.dev

```

### 3.4. Terminal User Interface (TUI)

Built with libraries like `ratatui` (Rust), the TUI is for power users.

- **Layout:**
- Left pane: Tree view of Git branches.
- Right pane: Live container logs.
- Bottom pane: System stats (CPU / RAM saved by Oxid).

- **Navigation:** Vim-style bindings (`j/k` to move up/down, `Enter` to wake/sleep, `/` to search branches).
- **Colors in TUI:** Background is transparent (uses the user's terminal bg). Borders of the active pane glow in **Oxid Orange**.

## 4. Iconography and Visuals

- **Style:** Line-art, monoline icons with sharp edges. No filled shapes unless it's a critical warning.
- **The Logo Concept:** A geometric, minimalist hexagon (representing a container or a port), with a missing top line to represent an open branch/fork. Drawn in Oxid Orange.
- **Gradients:** Avoid them. Use solid blocks of color. Rust design favors utility and flat colors over glossy web3-style gradients.

## 5. Tone of Voice (Copywriting)

The text inside the app, the CLI help menus, and the documentation should reflect the Rust community:

- **Direct & Helpful:** No corporate jargon. Instead of _"Optimal resource multiplexing engaged"_, use _"Connected to shared Postgres. Saved 1.2GB RAM."_
- **Empowering:** Treat the user as an expert. Offer `[--force]` flags, but clearly state what they overwrite.
- **Error Messages:** Following Rust’s famous compiler errors, Oxid’s errors must tell you _exactly_ what went wrong and _how_ to fix it.
- _Bad Error:_ `Config parse error.`
- _Oxid Error:_ `Error reading oxid.toml on line 12: Invalid duration '30'. Did you mean '30m' or '30s'?`

---
