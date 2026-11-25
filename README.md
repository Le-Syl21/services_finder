# Services Finder

`services.msc` can only jump to a service by the first letters of its display name.
Services Finder is a small native Windows program that lists the same services and
filters them as you type, across every column: display name, service name, status,
startup type, account, description and executable path.

Project page with screenshots: https://le-syl21.github.io/services_finder/

- **One file, about 245 KB.** Native Windows controls, no runtime to install, nothing
  written to the registry.
- **See what needs elevation.** A shield marks the services your account cannot start
  or stop without administrator rights; a padlock marks protected services that Windows
  does not let anyone stop. The answer comes from Windows itself: the program asks for
  the access and records whether it is granted.
- **Act from the list.** Right-click (or the menu key) to start, stop, restart, change
  the startup type, open the executable's folder or copy the name. When an action needs
  administrator rights, Windows asks (UAC) for that action only; the window itself keeps
  running as you.
- **Keyboard first.** `Ctrl+F` search, `↓` into the list, `Esc` clear, `F5` refresh.
  Several words must all match; `-word` excludes.

Windows 10 or later, 64-bit. English and French, following the Windows display language.

## Download

The signed `services_finder.exe` is on the
[releases page](https://github.com/Le-Syl21/services_finder/releases).

## Command line

- `services_finder --list` prints every service as tab-separated text, with the access
  the current account has (`can_control`, `can_config`, `protected`).
- `services_finder --do <start|stop|restart|auto|delayed|manual|disabled> <service>`
  performs one action and returns the Windows error code. The window uses it to run a
  single action elevated.

## Build

```sh
cargo build --release
```

On Linux, cross-compile with `x86_64-pc-windows-gnu` and MinGW-w64.

---

## En français

Services Finder liste les services Windows et les filtre pendant la frappe, sur toutes
les colonnes (nom, nom du service, état, type de démarrage, compte, description,
chemin). Un bouclier signale les services qu'on ne peut pas démarrer ou arrêter sans
droits administrateur, un cadenas ceux que Windows protège. Clic droit pour démarrer,
arrêter, redémarrer, changer le type de démarrage ou ouvrir l'emplacement du fichier :
l'UAC n'est demandé que pour l'action qui en a besoin. Un seul exe signé d'environ
245 Ko, sans installation.

Page du projet avec captures : https://le-syl21.github.io/services_finder/?lang=fr

## License

MIT
