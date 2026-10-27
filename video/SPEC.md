# Reason Network — 36 s launch film

Reference: @code_kartik's HyperFrames reel (light, quiet, centred type, one blue word, real things on screen).

## Look
- Ground `#F6F6F4`, ink `#111111`, muted `#8A8A86`, hairline `#E4E4E0`, accent blue `#2563FF`, ok green `#22C55E`, queued amber `#F59E0B`.
- Inter 500/600 for type, JetBrains Mono 500 for terminal lines. Headline 64 px, body 30 px, mono 26 px min.
- Cards: white, 1 px hairline, 18 px radius, shadow `0 20px 60px rgba(0,0,0,.06)`.
- Banned: dark gradient hero, neon glows, typing as opener, feature list as three cards, logo in a corner.

## Motion
- Entrances 0.5 s `power3.out`, y 24 → 0 with blur 8 → 0. Exits 0.35 s `power3.in`.
- Chip swaps 0.35 s, word-level accent colour flips at 0.25 s.
- Every message move = a real arrow along the QUIC link, 0.7 s `power2.inOut`.

## Shots (root seconds)
| # | t | Shot | Real source |
|---|---|---|---|
| 1 | 0.0–4.2 | "Your AI agents work **alone**." → chips of agent sessions cycle → "Reason Network" lands | product name |
| 2 | 4.2–10.2 | "Built with **Alan**" pill types the brief, real repo files cascade as a tilted wall; stat lines 14.8k lines of Rust / 126 tests / 29 modules | repo |
| 3 | 10.2–15.2 | Two daemons A and B. `team-create` → `team-…`; invite token (masked) travels to B; `joined team` | demo.ps1 run |
| 4 | 15.2–19.6 | `directory` on A: remote session `codex-on-B` appears with *remote* tag | demo.ps1 run |
| 5 | 19.6–25.0 | `send-message` A→B; bubble rides the QUIC link; B inbox fills; A outbox `pending → acked` | demo.ps1 run |
| 6 | 25.0–29.6 | B goes offline, message queued, B returns, delivered | demo.ps1 run |
| 7 | 29.6–32.6 | Claude Code calls `reason-network · list_team`, response lists sessions | MCP tool |
| 8 | 32.6–36.0 | Dot grid closes into a frame, wordmark, `Rust · iroh · QUIC · SQLite · MCP`, "Built with Alan" | — |

## Sound
No music source in this environment (no HeyGen sign-in): SFX only — whoosh on scene changes (two takes), pop on chips, typing on the brief, click on send, notification on delivery, bass impact on the wordmark.
