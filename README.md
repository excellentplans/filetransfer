# Datei übertragen (nascopy)

Kleines GUI-Werkzeug für Dolphin: Kontextmenü → **„Datei übertragen …“** → Zielordner
wählen → kopieren. Funktioniert für das **NAS** (`/mnt/nas`, CIFS) genauso wie für
**USB-Sticks** und lokale Ordner. Kopier-Engine ist `rsync` (resumable), die Oberfläche
ist Rust + egui.

## Installation

```sh
cargo build --release
cp target/release/nascopy ~/.local/bin/
cp nascopy.desktop ~/.local/share/kservices5/ServiceMenus/   # Pfade in Exec prüfen
kbuildsycoca5                                                # Menü neu einlesen
```

Danach erscheint **„Datei übertragen …“** im Rechtsklick-Menü von Dolphin (ohne Neustart).

## Ablauf

1. **Quellen**: kommen von Dolphin (`%F`, absolute Pfade, beliebig viele).
2. **Zielort wählen** (nur per Klick — keine Pfade tippen):
   - Seitenleiste: NAS · USB-Sticks (aus mountinfo) · Persönlicher Ordner
   - Brotkrumen-Navigation, **Aufwärts**, **Neuer Ordner …**, **Zuletzt verwendet**
   - Freier Platz wird angezeigt; nicht erreichbare Ziele führen zu einem
     Timeout-Hinweis (3 s), das Fenster friert nie ein.
3. **Konflikte** (gleicher Name am Ziel): pro Datei **Überspringen /
   Überschreiben / Umbenennen / Fortsetzen** (bei Teil-Kopie) plus „Für alle“.
   Umbenennen Dolphin-Stil: `name (2).ext`, `name (3).ext` …
4. **Übertragung**: rsync mit Fortschritt (%, MB/s, Restzeit, Datei-Zähler),
   **Abbrechen** bleibt fortsetzbar (`--partial --append-verify`).
5. **Verifizieren** (optional, gemerkter Haken, standardmäßig **an**):
   `rsync -rcn`-Gegenvergleich bzw. SHA-256 für zerlegte Dateien; fehlgeschlagene
   Dateien können erneut kopiert werden.
6. **Zusammenfassung** inkl. rsync-Statistik, optional „Fenster automatisch schließen“.

## FAT32-Erkennung & Aufteilen großer Videodateien

Erkennt die GUI am Ziel einen **FAT32-Datenträger** (z. B. viele USB-Sticks, Kamera-Karten):

- Dateien **über 4 GiB** können dort nicht liegen → sie werden beim Kopieren
  automatisch in **≤ 4-GiB-Teile** zerlegt: `urlaub.mkv.001`, `urlaub.mkv.002`, …
- Die Teile werden per **SHA-256 geprüft** und sind **fortsetzbar** (unterbrochene
  Kopie einfach erneut starten → „Fortsetzen“).
- Zum Zusammenführen auf dem Ziel (z. B. am Fernseher/PC mit FAT):
  `cat 'urlaub.mkv.0'* > 'urlaub.mkv'` — der Befehl steht in der Zusammenfassung
  und lässt sich kopieren.
- Auch **FAT-illegale Dateinamen** (`: * ? " < > | \`, Punkt/Leerzeichen am Ende)
  werden erkannt und automatisch umbenannt (`a:b` → `a_b`), inkl. der Dateien
  innerhalb kopierter Ordner.
- exFAT/NTFS haben kein 4-GiB-Limit und werden entsprechend behandelt.

## Sicherheitsnetz

- Löscht **nie** etwas (kein `--delete`).
- Verweigert Selbst-Kopier-Schleifen (Quelle im Zielbaum oder umgekehrt).
- **Nur eine Instanz** gleichzeitig (`flock`, `~/.local/state/nascopy.lock`);
  zweiter Start zeigt einen Hinweis statt zweier paralleler rsyncs.
- Schlägt etwas fehl (rsync fehlt, Ziel weg, …): lesbare Fehlermeldung + Log-Eintrag.

## Dateien

| Pfad | Zweck |
|---|---|
| `~/.local/bin/nascopy` | Programm |
| `~/.local/share/kservices5/ServiceMenus/nascopy.desktop` | Dolphin-Kontextmenü |
| `~/.local/state/nascopy.log` | Protokoll (Aufrufe, Entscheidungen, rsync-Exit-Codes, Verify) |
| `~/.local/state/nascopy.last` | Zuletzt verwendetes Ziel + Einstellungen (verify, autoclose) |
| `~/.local/state/nascopy.lock` | Single-Instance-Lock |

## Selbsttest ohne GUI

```sh
nascopy --selftest
```

Prüft progress2-Parser, Namensregeln, Lock, und macht eine komplette Kopierrunde
(rsync + FAT32-Zerlegung + Fortsetzen + Verify) in /tmp.

## Testplan (manuell gegen das NAS)

1. Rechtsklick auf eine Datei → Ablauf komplett durchlaufen.
2. Sonderzeichen: Datei mit Leerzeichen, Zielordner `Videos-2023-(SHOTCUT)`.
3. Konflikte: Überspringen lässt Zieldatei unangetastet; Überschreiben ersetzt
   Inhalt (--checksum); Umbenennen erzeugt `name (2).ext`; „Für alle“ wirkt.
4. Fortsetzen: ≥2-GB-Kopie starten, `pkill -f rsync`, erneut starten → matched data.
5. Verify OK: kopierte Datei stimmt.
6. Verify FEHLER: Datei am Ziel mit `dd` vermurksen → genau diese Datei wird gemeldet.
7. Abbrechen-Knopf mitten in der Übertragung → partial bleibt, „fortsetzbar“.
8. NAS offline: `sudo umount /mnt/nas` → freundlicher Fehler statt Einfrieren; danach remounten.
9. Doppelstart → Lock-Hinweis.
10. Log-Kontrolle in `~/.local/state/nascopy.log`.

## Bauen

Rust (stable), keine System-Qt-Abhängigkeiten. `cargo build --release` genügt.
Getestet mit Rust 1.99 / egui 0.36.
