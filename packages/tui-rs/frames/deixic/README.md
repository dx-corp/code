# First-launch Deixic frames

The startup and welcome widgets embed these literal ASCII frames with
`include_str!`. The application selects one of 36 frames every 80 ms. It does
not render geometry, decode images, or allocate a frame cache at startup.

The art is a beveled D aperture with a slow, closed rotation and fixed lighting.
Normal frames occupy 48 columns by 18 rows; compact frames occupy 40 by 12.
Both variants use the same phase and remain centered as the terminal resizes.
Reduced motion selects the first frame. Tiny terminals omit the art and retain
the action text.

To re-author the frames, run from the Maestro workspace:

```sh
python3 scripts/art/render-first-launch.py
python3 scripts/art/render-first-launch.py --compact
```

This offline authoring script uses only the Python standard library. Review
both sizes in the production widget before committing changed artwork:

```sh
COLORTERM=truecolor cargo run -p maestro-tui --example onboarding-preview
cargo test -p maestro-tui --lib components::startup
```

The boot animation represents pending workspace search only. It neither delays
completed preparation nor claims account, model, or tool readiness. The welcome
screen can continue showing the artwork while the user chooses to proceed.
