# README assets

- `task-flow.gif` — the README hero: a real session that diagnoses a failing
  test, edits the file, and re-runs the tests (compressed from a 25 s run).
- `screenshot-task.png` — the same session as a still.
- `screenshot-onboarding.png` — the last onboarding step.
- `screenshot-models.png` — the `/model` picker (provider → model → reasoning).
- `social-preview.png` — 1280×640 card for the repository's social preview.

## Social preview

GitHub has no API for it: upload `social-preview.png` by hand under
**Settings → General → Social preview**.

## Regenerating the screenshots

Capture a live TUI pane with ANSI color, then render it:

```sh
tmux -L shots new-session -d -s rec -x 140 -y 38 'bonsai'
tmux -L shots capture-pane -e -p -t rec > hero.ansi
scripts/render_terminal_screenshot.py hero.ansi hero.png
```

`scripts/render_terminal_screenshot.py` lays every cell out at a fixed grid,
paints the SGR background runs, and draws box-drawing and block characters as
geometry so borders and the bonsai art stay seamless. It needs Chrome or
Chromium for the PNG (use `--html-only` to skip it).

For the clip, capture frames on a timer and assemble them:

```sh
# frames are the same capture, taken every 0.5 s while the agent works
scripts/render_terminal_screenshot.py frame.ansi frame.png --scale 1
ffmpeg -nostdin -framerate 5 -i frame-%03d.png \
  -vf "split[a][b];[a]palettegen=max_colors=128:stats_mode=diff[p];[b][p]paletteuse=dither=none" \
  -loop 0 task-flow.gif
```
