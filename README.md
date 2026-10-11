# Dateien übertragen (filetransfer)

Kleines GUI-Werkzeug zum Kopieren von Dateien und Ordnern auf ein Ziel Ihrer
Wahl: **Netzwerk-Freigaben** (CIFS/NFS/SSHFS/GVfs), **USB-Sticks** oder
lokale Ordner. Die Kopier-Engine ist `rsync` (fortsetzbar), die Oberfläche
ist Rust + egui.

Zwei Wege, es zu benutzen:

- **Dolphin**: Dateien markieren → Kontextmenü → **„Dateien übertragen …“**
- **Eigenständig**: Programm starten → Dateien per **Drag & Drop** hineinziehen
  oder über **„Dateien wählen …“** auswählen

## Installation

```sh
cargo build --release
cp target/release/filetransfer ~/.local/bin/
cp filetransfer.desktop ~/.local/share/kservices5/ServiceMenus/   # Pfad in Exec prüfen
kbuildsycoca5                                                     # Menü neu einlesen
```

Danach erscheint **„Dateien übertragen …“** im Rechtsklick-Menü von Dolphin
(ohne Neustart). Beim ersten Start übernimmt das Programm das zuletzt
verwendete Ziel des Vorgängers `nascopy`.

## Ablauf

1. **Quellen**: von Dolphin (`%F`) oder per Drag & Drop / Dateiauswahl.
2. **Zielort wählen** (nur per Klick — keine Pfade tippen):
   - Seitenleiste: Persönlicher Ordner · USB-Sticks (aus mountinfo) ·
     Netzwerk-Freigaben (CIFS, NFS, SSHFS, GVfs) · „Anderer Ordner …“
     für beliebige Pfade
   - Brotkrumen-Navigation, **Nach oben**, **Neuer Ordner …**,
     **Zuletzt verwendet**
   - Freier Platz wird angezeigt; nicht erreichbare Ziele führen zu einem
     Timeout-Hinweis (3 s), das Fenster friert nie ein.
3. **Konflikte** (gleicher Name am Ziel): pro Datei **Überspringen /
   Überschreiben / Umbenennen / Fortsetzen** (bei Teil-Kopie) plus
   „Für alle“. Umbenennen Dolphin-Stil: `name (2).ext`, `name (3).ext` …
4. **Übertragung**: rsync mit Fortschrittsbalken (grün), Geschwindigkeit,
   Datei-Zähler und großem Fertig-Symbol. **Abbrechen** bleibt fortsetzbar
   (`--partial --append-verify`).
5. **Verifizieren** (optional, gemerkter Haken, standardmäßig **an**):
   während der Prüfung erscheint ein zweiter, blauer Fortschrittsbalken.
   `rsync -rcn`-Gegenvergleich bzw. SHA-256 für zerlegte Dateien;
   fehlgeschlagene Dateien können erneut kopiert werden.
6. **Zusammenfassung** inkl. rsync-Statistik, optional
   „Fenster automatisch schließen“.

## FAT32-Erkennung & Aufteilen großer Videodateien

Erkennt die GUI am Ziel einen **FAT32-Datenträger** (z. B. viele USB-Sticks,
Kamera-Karten):

- Dateien **über 4 GiB** können dort nicht liegen → sie werden beim Kopieren
  automatisch in **≤ 4-GiB-Teile** zerlegt: `urlaub.mkv.001`,
  `urlaub.mkv.002`, …
- Die Teile werden per **SHA-256 geprüft** und sind **fortsetzbar**
  (unterbrochene Kopie einfach erneut starten → „Fortsetzen“).
- Zum Zusammenführen auf dem Ziel (z. B. am Fernseher/PC mit FAT):
  `cat 'urlaub.mkv.0'* > 'urlaub.mkv'` — der Befehl steht in der
  Zusammenfassung und lässt sich kopieren.
- Auch **FAT-illegale Dateinamen** (`: * ? " < > | \`, Punkt/Leerzeichen am
  Ende) werden erkannt und automatisch umbenannt (`a:b` → `a_b`), inklusive
  der Dateien innerhalb kopierter Ordner.
- exFAT/NTFS haben kein 4-GiB-Limit und werden entsprechend behandelt.

## Sicherheitsnetz

- Löscht **nie** etwas (kein `--delete`).
- Verweigert Selbst-Kopier-Schleifen (Quelle im Zielbaum oder umgekehrt).
- **Nur eine Instanz** gleichzeitig (`flock`,
  `~/.local/state/filetransfer.lock`); zweiter Start zeigt einen Hinweis
  statt zweier paralleler rsyncs.
- Schlägt etwas fehl (rsync fehlt, Ziel weg, …): lesbare Fehlermeldung
  + Log-Eintrag.

## Dateien

| Pfad | Zweck |
|---|---|
| `~/.local/bin/filetransfer` | Programm |
| `~/.local/share/kservices5/ServiceMenus/filetransfer.desktop` | Dolphin-Kontextmenü |
| `~/.local/state/filetransfer.log` | Protokoll (Aufrufe, Entscheidungen, rsync-Exit-Codes, Verify) |
| `~/.local/state/filetransfer.last` | Zuletzt verwendetes Ziel + Einstellungen (verify, autoclose) |
| `~/.local/state/filetransfer.lock` | Single-Instance-Lock |

## Selbsttest ohne GUI

```sh
filetransfer --selftest
```

Prüft progress2-Parser, Namensregeln, Lock, und macht eine komplette
Kopierrunde (rsync + FAT32-Zerlegung + Fortsetzen + Verify) in /tmp.

## Testplan (manuell)

1. Rechtsklick auf eine Datei → Ablauf komplett durchlaufen.
2. Eigenständig starten: Drag & Drop aus Dolphin, dann „Dateien wählen …“.
3. Sonderzeichen: Datei mit Leerzeichen, Zielordner `Videos-2023-(SHOTCUT)`.
4. Konflikte: Überspringen lässt Zieldatei unangetastet; Überschreiben
   ersetzt Inhalt (--checksum); Umbenennen erzeugt `name (2).ext`;
   „Für alle“ wirkt.
5. Fortsetzen: ≥2-GB-Kopie starten, `pkill -f rsync`, erneut starten →
   matched data.
6. Verify OK: kopierte Datei stimmt; blauer Prüfbalken erscheint während
   der Prüfung.
7. Verify FEHLER: Datei am Ziel mit `dd` vermurksen → genau diese Datei
   wird gemeldet.
8. Abbrechen-Knopf mitten in der Übertragung → partial bleibt,
   „fortsetzbar“.
9. Ziel offline: `sudo umount /mnt/nas` → das Ziel verschwindet aus der
   Seitenleiste; nach dem Remount erscheint es wieder.
10. Doppelstart → Lock-Hinweis.
11. Log-Kontrolle in `~/.local/state/filetransfer.log`.

## Bauen

Rust (stable), keine System-Qt/GTK-Abhängigkeiten (Dateiauswahl läuft über
das XDG-Portal). `cargo build --release` genügt. Getestet mit
Rust 1.99 / egui 0.36.
