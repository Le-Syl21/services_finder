# Changelog

## 0.1.0 (unreleased)

First version.

- Lists every Win32 service and filters as you type across all columns (display name,
  service name, status, startup type, account, description, path). Several words must
  all match; `-word` excludes.
- Shield on services the current account cannot start or stop without administrator
  rights, padlock on protected services, both probed from Windows itself.
- Context menu: start, stop, restart, startup type (automatic, delayed, manual,
  disabled), open file location, copy names. Actions that need administrator rights
  ask UAC for that action only.
- `--list` prints the services and access rights as tab-separated text; `--do` runs one
  action.
- English and French. One native exe, no runtime.
