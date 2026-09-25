---
name: Obsidian Telemetry Engine
colors:
  surface: '#101419'
  surface-dim: '#101419'
  surface-bright: '#36393f'
  surface-container-lowest: '#0a0e14'
  surface-container-low: '#181c21'
  surface-container: '#1c2025'
  surface-container-high: '#262a30'
  surface-container-highest: '#31353b'
  on-surface: '#e0e2ea'
  on-surface-variant: '#bcc9cd'
  inverse-surface: '#e0e2ea'
  inverse-on-surface: '#2d3136'
  outline: '#869397'
  outline-variant: '#3d494c'
  surface-tint: '#4cd7f6'
  primary: '#4cd7f6'
  on-primary: '#003640'
  primary-container: '#06b6d4'
  on-primary-container: '#00424f'
  inverse-primary: '#00687a'
  secondary: '#d0bcff'
  on-secondary: '#3c0091'
  secondary-container: '#571bc1'
  on-secondary-container: '#c4abff'
  tertiary: '#ffb95f'
  on-tertiary: '#472a00'
  tertiary-container: '#e79400'
  on-tertiary-container: '#563400'
  error: '#ffb4ab'
  on-error: '#690005'
  error-container: '#93000a'
  on-error-container: '#ffdad6'
  primary-fixed: '#acedff'
  primary-fixed-dim: '#4cd7f6'
  on-primary-fixed: '#001f26'
  on-primary-fixed-variant: '#004e5c'
  secondary-fixed: '#e9ddff'
  secondary-fixed-dim: '#d0bcff'
  on-secondary-fixed: '#23005c'
  on-secondary-fixed-variant: '#5516be'
  tertiary-fixed: '#ffddb8'
  tertiary-fixed-dim: '#ffb95f'
  on-tertiary-fixed: '#2a1700'
  on-tertiary-fixed-variant: '#653e00'
  background: '#101419'
  on-background: '#e0e2ea'
  surface-variant: '#31353b'
typography:
  headline-lg:
    fontFamily: Geist
    fontSize: 28px
    fontWeight: '600'
    lineHeight: 34px
    letterSpacing: -0.02em
  headline-lg-mobile:
    fontFamily: Geist
    fontSize: 22px
    fontWeight: '600'
    lineHeight: 28px
    letterSpacing: -0.01em
  headline-md:
    fontFamily: Geist
    fontSize: 20px
    fontWeight: '600'
    lineHeight: 26px
    letterSpacing: -0.01em
  headline-sm:
    fontFamily: Geist
    fontSize: 16px
    fontWeight: '600'
    lineHeight: 22px
    letterSpacing: -0.005em
  body-lg:
    fontFamily: Geist
    fontSize: 14px
    fontWeight: '400'
    lineHeight: 20px
  body-md:
    fontFamily: Geist
    fontSize: 13px
    fontWeight: '400'
    lineHeight: 18px
  body-sm:
    fontFamily: Geist
    fontSize: 12px
    fontWeight: '400'
    lineHeight: 16px
  code-lg:
    fontFamily: JetBrains Mono
    fontSize: 13px
    fontWeight: '500'
    lineHeight: 18px
    letterSpacing: -0.01em
  code-md:
    fontFamily: JetBrains Mono
    fontSize: 12px
    fontWeight: '400'
    lineHeight: 16px
  code-sm:
    fontFamily: JetBrains Mono
    fontSize: 11px
    fontWeight: '400'
    lineHeight: 14px
  label-md:
    fontFamily: JetBrains Mono
    fontSize: 11px
    fontWeight: '600'
    lineHeight: 14px
    letterSpacing: 0.04em
  label-sm:
    fontFamily: JetBrains Mono
    fontSize: 10px
    fontWeight: '600'
    lineHeight: 12px
    letterSpacing: 0.06em
rounded:
  sm: 0.125rem
  DEFAULT: 0.25rem
  md: 0.375rem
  lg: 0.5rem
  xl: 0.75rem
  full: 9999px
spacing:
  gutter: 0.5rem
  gutter-desktop: 0.75rem
  margin: 0.75rem
  margin-desktop: 1rem
  space-xs: 0.125rem
  space-sm: 0.25rem
  space-md: 0.5rem
  space-lg: 0.75rem
  space-xl: 1rem
---

## Brand & Style

This design system targets systems engineers, infrastructure architects, and compiler runtime operators who monitor compilation and retrieval middleware pipelines in mission-critical environments.

The visual direction merges terminal UI (TUI) rigor with modern desktop IDE ergonomics. It borrows structural framing, low-latency visual cadence, and tabular density from Ratatui, paired with the precision surfaces of a production dark workstation. Visual noise is stripped away; every pixel delivers diagnostic value. 

Core styling tenets:
- **Zero-fluff utilitarianism**: Information hierarchy relies on micro-borders, mono-spaced alignment grids, and purposeful telemetry accents rather than ambient blur or decorative illustrations.
- **Instrument-grade feedback**: Real-time status states (heartbeats, write-ahead log flush latencies, semantic routing graphs) use crisp, phosphor-inspired chromatic cues against absolute carbon and obsidian baselines.
- **Hardware-adjacent precision**: Borders resemble milled chassis seams; status badges mimic hardware bus indicators and micro-LED panels.

## Colors

The palette operates in high-contrast dark space, anchored by a deep obsidian background (`#090d12`) with micro-contrasted surface layers (`#0e1520`, `#141d2b`, `#1c273a`). 

Functional role assignments:
- **Primary (`#06b6d4` / `#0ea5e9`)**: Retrieval telemetry, streaming query execution channels, active compilation units, and focus rings.
- **Secondary (`#8b5cf6` / `#a855f7`)**: Semantic graph topologies, QUG vector embeddings, and reciprocal rank fusion (RRF) stages.
- **Tertiary (`#f59e0b`)**: Compiler execution budget boundaries, lease heartbeats, and throttle thresholds.
- **Success / Healthy (`#10b981`)**: Kernel WAL flush confirmations, active socket health, and schema validation integrity.
- **Destructive / Error (`#f43f5e`)**: Dead-letter queues, memory lease drops, schema violations, and unrecoverable pipeline traps.
- **Neutral Scales**:
  - `canvas`: `#06080b`
  - `surface-base`: `#090d12`
  - `surface-panel`: `#0f1722`
  - `surface-elevated`: `#172131`
  - `border-subtle`: `#1f2c3f`
  - `border-strong`: `#33445c`
  - `text-bright`: `#f1f5f9`
  - `text-muted`: `#94a3b8`
  - `text-dim`: `#475569`

## Typography

The typographic hierarchy is split into two operational modes:
1. **Structural Sans (`Geist`)**: Used for workspace module headings, layout frames, contextual system labels, and prose documentation. Provides crisp rendering at high display densities without taking horizontal real estate away from metrics.
2. **Telemetry Monospace (`JetBrains Mono`)**: Powers numeric counters, byte layouts, hex dumps, pipeline nodes, trace IDs, and query tokens. Enforces tabular alignment across multi-row metrics without optical jitter during live streaming updates.

All labels and micro-badges use uppercase `JetBrains Mono` with expanded tracking (`0.04em` to `0.06em`) to preserve legibility at micro-scales (10px–11px).

## Layout & Spacing

The layout philosophy follows a multi-pane tiling model inspired by IDEs and Ratatui terminal splitters. Content is arranged edge-to-edge with thin borders separating adjacent functional zones, maximizing viewport density for operational data.

- **Desktop (>= 1280px)**: 24-column modular workspace with configurable sidecar panels (telemetry trace inspector, WAL buffer status, semantic query canvas). Gutters remain tight (`0.75rem`) to maximize information density.
- **Tablet / Laptop (768px - 1279px)**: 12-column adaptive layout. Sub-graphs and log streams collapse into tabbed horizontal panes.
- **Mobile (< 768px)**: Single column stacked stack. Secondary metric columns in data tables collapse into expandable drawer inspectors.
- **Rhythm**: Spacing follows a 4px base increment (`0.125rem`, `0.25rem`, `0.5rem`, `0.75rem`, `1rem`). Internal component padding uses `space-xs` and `space-sm` for dense tables, expanding to `space-md` for structural cards.

## Elevation & Depth

This design system deliberately eschews diffuse drop shadows. Depth and visual hierarchy are communicated strictly through:
- **Tonal Stepping**: 
  - Level 0 (Base Canvas): `#06080b`
  - Level 1 (Workstation Panes / Dockers): `#090d12`
  - Level 2 (Inspector Cards / Section Grids): `#0f1722`
  - Level 3 (Active Flyouts, Context Menus, Modals): `#172131`
- **Ghost Borders**: Structural dividers use crisp 1px borders with explicit hex values (`#1f2c3f` for muted containers, `#33445c` for focused panes).
- **Telemetry Glow**: Active and warning states use razor-sharp 1px border highlights paired with a tight, restrained outline (e.g., `0 0 0 1px #06b6d4`, or `0 0 8px rgba(6, 182, 212, 0.2)` on critical alert triggers), replicating terminal phosphor backlights.

## Shapes

The design system maintains a hard-edged, technical geometry (`roundedness: 1`). 

- Default elements (buttons, inputs, status badges, metric chips) use a minimal `0.25rem` (4px) radius to soften terminal harshness without breaking the IDE tiling grid.
- Panes, cards, telemetry tables, and split views use sharp corners (`0px` or `2px`) when docked against adjacent panels to preserve unified dividing lines.
- Modals, tooltips, and floating diagnostic menus use `rounded-lg` (`0.5rem`) with 1px border delineation to clearly isolate floating overlays from underlying data grids.

## Components

### Buttons
- **Primary (Action / Run)**: Background `#06b6d4`, foreground `#06080b`, font `label-md`. Hover shifts to `#0ea5e9`. Active state contracts by 1px.
- **Secondary / Console**: Background `#0f1722`, border 1px solid `#1f2c3f`, text `#f1f5f9`. Hover changes border to `#33445c` and background to `#172131`.
- **Destructive (Purge / Evict)**: Background `transparent`, border 1px solid `#f43f5e`, text `#f43f5e`. Hover fills with `rgba(244, 63, 94, 0.1)`.
- **Icon / Action Trigger**: Square (28px × 28px or 24px × 24px), mono-spaced, padding `space-xs`.

### Telemetry Badges & Pipeline Pills
- Built with uppercase `JetBrains Mono` (`label-sm`), 18px fixed height, padding `0.125rem 0.375rem`.
- **Healthy / WAL Valid**: Emerald background `rgba(16, 185, 129, 0.1)`, text `#10b981`, border `rgba(16, 185, 129, 0.3)`.
- **Budget / Lease Warning**: Amber background `rgba(245, 158, 11, 0.1)`, text `#f59e0b`, border `rgba(245, 158, 11, 0.3)`. Includes a pulse indicator dot (`5px × 5px`).
- **Graph / Fusion**: Purple background `rgba(139, 92, 246, 0.12)`, text `#a855f7`, border `rgba(139, 92, 246, 0.35)`.
- **Critical / DLQ**: Crimson background `rgba(244, 63, 94, 0.12)`, text `#f43f5e`, border `rgba(244, 63, 94, 0.4)`.

### Form Controls (Inputs, Checkboxes, Switches)
- **Input Fields**: Background `#090d12`, border 1px solid `#1f2c3f`, text `#f1f5f9`, font `code-md`. Focus applies 1px solid `#06b6d4` with zero drop-shadow spread. Prefix indicators (e.g., `query>`, `wal:`) rendered in `#475569`.
- **Checkboxes**: 14px × 14px square, radius 2px, border 1px solid `#33445c`, checked background `#06b6d4` with contrasting dark check indicator.

### Data Grids & Structured Tables
- Monospace-aligned tabular figures (`font-variant-numeric: tabular-nums`).
- Sticky headers with `#090d12` background, bottom border 1px solid `#1f2c3f`, text `label-sm` in `#94a3b8`.
- Rows use fixed heights (28px compact, 36px standard), hover highlight `#0f1722`. Active selections display a 2px vertical cyan border strip on the leftmost edge.

### Workstation Cards & Metric Gauges
- **Cards**: Flat `#0f1722` background, 1px solid border `#1f2c3f`. Header actions separated by a 1px baseline border.
- **Resource Gauge**: 4px horizontal split bars. Segments render with solid fills: healthy allocations in `#06b6d4`, compiler budgets in `#f59e0b`, and buffer overruns in `#f43f5e`. Track background `#141d2b`.