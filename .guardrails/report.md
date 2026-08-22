# Guardrails Report

Project: xomposite
Generated at: 2026-08-22T19:57:34.856Z
Score geral: 82/100

Findings: 1

## Médio

- src/graphics/renderer.rs, src/graphics/renderer.rs
  - Regra: similar_functions
  - Política: warn
  - Status: new
  - Problema: Functions shader_log, program_log are structurally similar.
  - Ação sugerida: Review whether the shared structure is intentional or should be extracted.
