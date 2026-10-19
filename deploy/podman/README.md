# Podman Quadlet — run this folder as a systemd service.

Two units are provided:

| File | What it does |
| --- | --- |
| `unloot.container` | One-shot. Start it, it sorts whatever is in the folder, it exits. |
| `unloot-watch.container` | Watcher. Stays up and picks up photos as they appear. |
| `unloot.volume` | The photo folder as a podman volume, so permissions are podman's problem. |

## One-time setup

```sh
# 1. Build the image (podman builds the same Dockerfile).
podman build -t unloot .

# 2. Install the units.
mkdir -p ~/.config/containers/systemd
cp deploy/podman/unloot.container deploy/podman/unloot.volume ~/.config/containers/systemd/

# 3. Secrets and endpoint, out of git and where only you can read them.
cat > ~/.config/unloot.env <<'EOF'
UNLOOT_LLM_URL=http://192.168.137.134:8010/v1
UNLOOT_LLM_API_KEY=
EOF
chmod 600 ~/.config/unloot.env

# 4. Decide where your photos live: either the named volume (default), or your
#    own folder. For your own folder, edit the Volume line in unloot.container:
#      Volume=%h/Vinted:/workdir:rw
systemctl --user daemon-reload
```

The volume a quadlet `.volume` file creates is named after the unit, so if you
keep `unloot.volume` the container line above already matches it. Check with
`podman volume ls`.

## Run it

```sh
systemctl --user start unloot.service            # one pass
journalctl --user -u unloot.service -f           # watch it work
podman unchown ...                               # only if you fight over ownership
```

Drop photos in, then start it again — processed photos are remembered in the
ledger, so a second start only handles the new ones.

## Or run it on a timer instead of watching

A oneshot unit plus a timer is cheaper to live with than a watcher that never
exits:

```ini
# ~/.config/containers/systemd/unloot.timer
[Unit]
Description=Sort Vinted photos every 15 minutes

[Timer]
OnUnitInactiveSec=15m
OnBootSec=5m

[Install]
WantedBy=timers.target
```

```sh
systemctl --user daemon-reload && systemctl --user enable --now unloot.timer
```

## Watcher

```sh
cp deploy/podman/unloot-watch.container ~/.config/containers/systemd/
systemctl --user daemon-reload && systemctl --user --now enable unloot-watch.service
```

It needs `--settle` to be larger than the time your phone takes to copy a photo
over, or it will grab a half-written file. The default (20 s) is generous.

## Notes

- `Type=oneshot` with `TimeoutStartSec=infinity`: a few hundred photos through a
  local model is slow, and being killed halfway is the worst outcome.
- The units ask for a read-only container with no extra capabilities. `/workdir`
  is a volume, so it stays writable.
- The state ledger lives inside the volume at `/workdir/.unloot/state.json`. If
  you blow the volume away, so does your memory of what was already sorted.
