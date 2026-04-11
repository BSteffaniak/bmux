# Distribution

bmux distribution is channel-based and hosted on Cloudflare at:

- `https://packages.bmux.dev/stable`
- `https://packages.bmux.dev/nightly`

## Worker endpoints

- `GET /install` (defaults to stable)
- `GET /install?channel=nightly`
- `GET /channels.json`

## APT

APT repositories are exposed at:

- `https://packages.bmux.dev/stable/apt`
- `https://packages.bmux.dev/nightly/apt`

Example source entry:

```text
deb [arch=amd64] https://packages.bmux.dev/stable/apt stable main
```

## RPM

RPM repositories are exposed at:

- `https://packages.bmux.dev/stable/rpm`
- `https://packages.bmux.dev/nightly/rpm`

Example repo file:

```ini
[bmux-stable]
name=bmux stable
baseurl=https://packages.bmux.dev/stable/rpm
enabled=1
gpgcheck=1
```

## npm packages

- Main packages: `bmux`, `@bmux/cli`
- Platform packages: `@bmux/<platform>`

See `npm/` and `.github/workflows/release-npm.yml`.
