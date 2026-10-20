# OpenFault — working rules

## Design system is a mandate
Source of truth: https://claude.ai/artifact/VtHQPd6CoYTft3Lu1McsKi (Design System artifact; read `project/README.md` and `project/tokens.json` from it before UI work).
Any new UI component or visual change MUST:
- Use only the tokens in `static/app.css` (same values as the artifact's tokens.json). No new colors; black/white only, `--destructive` and `--green` only for state, never decoration.
- Reuse existing component CSS (pill buttons/inputs 36px, cards 24px padding `--r-card`, sidebar rows 30px, top bar 56px) before writing new rules.
- Borders never ≥2px. Focus/selection = subtle bg tint + existing 1px line. focus-visible = 1px inset ring at most. No heavy shadow halos.
- Motion ≤250ms via `--t`, respects prefers-reduced-motion.
- Icons: Phosphor regular from `static/icons.svg`; add only icons actually used.
- Both themes (`:root` + `.dark`) every time.
If a component is new, add it to the Design System artifact (README + preview) in the same task.

## Logo
`static/mark.svg` is the mark (fault-block, fill currentColor). Use it inline wherever the old CSS `.mark` was; wordmark stays `.wordmark` text. Mono only, never recolored, min 16px, clear space = 1× mark height. Favicon derives from the same SVG.
