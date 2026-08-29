# Milestone 3a3f8a — Interactive Geometry Fast Path

Status: released and pushed.

## Release

- Commit: `b60eca9a1a9269b74d6f7d8cf90016dacaa67159`
- Tag: `milestone-3a3f8a`
- Remote: `origin/main` points to the release commit.
- Release subject: `perf(x11): add interactive geometry fast path`
- Production file changed: `src/x11/scene.rs`
- Final validation: 340 tests passed; check, `-Dwarnings`, build, and diff-check passed.
- Human runtime acceptance: **EXCELLENT**; residual stutter was almost imperceptible.

## What was delivered

The compositor now carries the latest ConfigureNotify geometry instead of
immediately reducing it to a generic invalidation. For a strict pure move—same
surface, semantic client, root, dimensions, depth, visual, lifecycle, and
stacking assumptions, with only x/y changed—the existing LiveScene is updated
without reacquiring resources.

The pure MoveOnly path preserves:

- XDamage ownership and the exact-once DamageSubtract obligation;
- NamedPixmap ownership and RAII cleanup;
- EGL imports and GL resources;
- border, shadow, opacity, fullscreen, and explicit blur semantics;
- Regions blur ClientRootGeometry through cached `dx/dy` translation;
- old LiveScene authority until a candidate is successfully published.

ConfigureNotify handling was refined so a known semantic-client event is
normalized to its owning surface geometry. Known safely non-renderable windows
may be ignored, while mapped/offscreen-pruned surfaces remain discoverable and
unknown unsafe windows remain conservative. Map, Unmap, Create, Destroy,
Reparent, and lifecycle changes remain structural. Resize remains
non-rebaseable. `MAX_CANDIDATE_RETRIES` remains `1`.

An in-flight structural candidate can absorb a newer compatible pure x/y move
before publication. Only placement metadata changes; resource dimensions,
pixmaps, Damage handles, EGL imports, and lifecycle state are not rewritten.
Incompatible geometry, resize, stacking, identity, reparent, unmap, or destroy
conditions retain the old stale/retry behavior.

## Performance evidence

| Stage | MoveOnly | Structural candidates | Stale candidates | Moving-surface stale Geometry |
|---|---:|---:|---:|---:|
| Early runtime | 0 successes | Hierarchy dominated | — | — |
| 3a3f8a8 | 278/279 (99.64%) | 24 | 27 | 24 |
| 3a3f8a10 | 717/717 (100%) | 9 | 3 | 0 |

The 3a3f8a10 session also measured 787 recompositions and 728 Present
submissions. These figures are observations, not a claim that every
recomposition is wasted.

The improvement came in stages:

1. resource-preserving MoveOnly with zero move-induced validation round trips;
2. ConfigureNotify ownership normalization, which removed Hierarchy starvation;
3. in-flight pure-move candidate rebasing, which removed moving-surface stale
   candidate amplification.

## Zero-roundtrip/resource contract

For a successful pure MoveOnly cycle, the move-induced deltas are:

| Operation | Delta |
|---|---:|
| GetGeometry | 0 |
| GetWindowAttributes | 0 |
| GetInputFocus | 0 |
| TranslateCoordinates | 0 |
| QueryTree | 0 |
| property/ownership validation replies | 0 |
| Damage reacquisition | 0 |
| NamedPixmap reacquisition | 0 |
| EGL reimport | 0 |
| unrelated surface rebuild | 0 |

Normal asynchronous rendering, XDamage, EGL, GL, and Present work remains
allowed; the contract concerns work caused by the pure move itself.

## Diagnostic history and cleanup

Temporary diagnostics were used during investigation and removed before
release:

- `3a3f8a3`: aggregate runtime counters for ConfigureNotify, MoveOnly,
  candidates, recomposition, and Present;
- `3a3f8a5`: bounded ConfigureNotify/Hierarchy attribution;
- `3a3f8a7`: event-derived lineage attribution and bounded parent walks.

The final production tree contains none of the diagnostic counters, sample
logging, session summary, or diagnostic lineage registry. Production behavior
and coverage were retained; final count is 340 tests.

## Audits and artifacts

- Canonical patch:
  `/home/demostenes/Desenvolvimento/xomposite-design-patches/milestone-3a3f8a-interactive-geometry-fast-path-canonical.patch`
  SHA256 `1eb5810682106958aa3d0336f6b0650d74e094c5d0119f4db609a55efc9377b0`
- Final residual audit:
  `/home/demostenes/Desenvolvimento/xomposite-design-reports/milestone-3a3f8a11-final-residual-structural-rebuild-audit.txt`
  SHA256 `99c8a96c094fa8b1e0b49d827cfc166b829acbd1adcaba81b8dc2d6e5a13d5c3`
- Release record:
  `/home/demostenes/Desenvolvimento/xomposite-design-reports/milestone-3a3f8a-release.txt`
  SHA256 `b2813690de6b76b3551eaf53bea1d2b462e6ae94fa779670dd573266f83c402c`
- Release binary:
  `/home/demostenes/Desenvolvimento/xomposite/target/debug/xlibre-compositor-poc`
  SHA256 `a5975e0d4f6cd09692c4bd018c7d24d14322c99b9012989181d661427c59fe0f`

## Known boundary

The accepted path still redraws the complete scene for a MoveOnly update.
Legitimate structural candidates can reacquire Damage, NamedPixmap, and EGL
resources for unchanged surfaces. The final audit found no narrow avoidable bug
that justified delaying release; this is broader follow-up work.

## Next milestone

`3a3f8b — resource-preserving structural candidate reuse`

The next phase should preserve lifecycle authority, transactional LiveScene
publication, exact-once DamageSubtract obligations, NamedPixmap RAII, EGL
ownership, and resize/stacking safeguards. It must not be folded back into
3a3f8a.

## Final smoke-test checklist

After any future change, run the compositor built from the candidate and verify:

- floating pure move, including horizontal, vertical, and diagonal drags;
- resize, explicitly treated as a separate path;
- tiled ↔ floating transitions;
- window creation and destruction;
- explicitly requested blur and Regions blur;
- transparent clients without blur remain without blur;
- no stale frames, clipping, border/shadow lag, crashes, or resource errors.

Do not optimize resize, partial redraw, stacking, or Present pacing under the
3a3f8 scope.
