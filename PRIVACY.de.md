# Datenschutzerklärung

*[English version: PRIVACY.md](PRIVACY.md) · Stand: 5. November 2026*

Diese Erklärung gilt für die bookmoor-Browser-Erweiterung (Chrome und Firefox) und die bookmoor-Serversoftware. Sie gilt nicht für eine Website; die Projektseite ist das [GitHub-Repository](https://github.com/swallat/bookmoor).

## 1. Datenschutz auf einen Blick

- bookmoor synchronisiert Ihre Lesezeichen zwischen Ihren Browsern über **einen Server, den Sie selbst betreiben** (oder den jemand betreibt, dem Sie vertrauen).
- **Der Entwickler erhält keine Daten.** In der Erweiterung und im Server gibt es keinen vom Entwickler betriebenen Server, kein Konto, keine Analyse, keine Werbung und kein Tracking.
- Die Erweiterung sendet Daten **nur an die Serveradresse, die Sie in den Einstellungen eintragen**. Sie nimmt zu keiner anderen Seite Kontakt auf.
- bookmoor tut nichts, solange Sie keine Serveradresse und kein API-Token eingetragen haben.

## 2. Verantwortliche Stelle

Für diese Software und diese Erklärung:

Sebastian Wallat - voidyapp.ing
E-Mail: hello@voidyapp.ing

**Verantwortlich für die Daten auf Ihrem bookmoor-Server ist der Betreiber dieses Servers.** Betreiben Sie ihn selbst, sind Sie das. Betreibt ihn jemand anderes für Sie, fragen Sie dort nach dem Umgang mit den Daten.

## 3. Welche Daten die Erweiterung verarbeitet

| Daten | Zweck | Wohin |
| --- | --- | --- |
| Ihre Lesezeichen (Titel, URLs, Ordnerstruktur, Reihenfolge) | Abgleich zwischen Ihren Browsern | An Ihren bookmoor-Server |
| Das eingetragene API-Token | Anmeldung am Server | An Ihren bookmoor-Server (als `Authorization`-Header) |
| Eingetragene zusätzliche Header und Query-Parameter (z. B. für einen Reverse Proxy) | Durchkommen durch Ihren eigenen Proxy oder Share-Link | An Ihren bookmoor-Server |
| Serveradresse, Token, zusätzliche Header, Sync-Zustand (welches lokale Lesezeichen zu welchem Server-Eintrag gehört), letzter Sync-Status | Damit die Erweiterung funktioniert | Nur im Browser (`storage`) auf Ihrem Gerät |

Die Erweiterung benötigt diese Browser-Berechtigungen:

- `bookmarks`: Lesezeichenbaum lesen und Änderungen aus anderen Browsern übernehmen.
- `storage`: Einstellungen und den oben beschriebenen Sync-Zustand speichern.
- `alarms`: Uploads kurz nach einer Änderung und regelmäßige Abfragen planen.
- Zugriff auf die eingetragene Serveradresse (wird beim Speichern der Einstellungen als optionale Host-Berechtigung angefragt): zur Kommunikation mit Ihrem Server und nur mit diesem.

Die Erweiterung enthält keinen nachgeladenen Code und lädt zur Laufzeit keine Bibliotheken von Dritten.

## 4. Welche Daten der Server verarbeitet

Der Server speichert den empfangenen Lesezeichenbaum, eine Liste der Geräte (Endpunkte), die gehashten API-Tokens und ein Änderungsprotokoll in einer JSON-Datei auf dem Rechner, auf dem er läuft. Er schreibt ein Anfrageprotokoll (Zeit, Methode, Pfad, Status, Token-Name und die beim Server sichtbare Client-Adresse) in die Konsolenausgabe. Wohin diese Ausgabe geht und wie lange sie aufbewahrt wird, hängt davon ab, wie Sie den Server betreiben. Der Server sendet nichts an Dritte.

## 5. Empfänger und Drittländer

bookmoor gibt Daten an niemanden weiter. Ihre Lesezeichen gehen **nur an Ihren eigenen Server**. Schalten Sie einen Reverse Proxy, einen Tunnel oder einen Hosting-Anbieter davor, können diese den Verkehr sehen, den Sie darüber leiten. Wählen Sie sie entsprechend aus und nutzen Sie HTTPS.

Datenübermittlungen in Drittländer finden nur statt, wenn Sie Ihren Server dort betreiben.

## 6. Bezugswege

Sie können die Erweiterung aus dem Chrome Web Store, von Mozilla Add-ons oder aus einem Build des GitHub-Repositorys installieren. Diese Dienste (Google, Mozilla, GitHub) verarbeiten nach ihren eigenen Datenschutzerklärungen eigene Daten, etwa Download-Statistiken oder Ihre IP-Adresse. Der Entwickler hat darüber keinen Zugriff auf Ihre bookmoor-Daten.

## 7. Rechtsgrundlagen (DSGVO)

Soweit der Entwickler Verantwortlicher ist (Kontakt per E-Mail, Abschnitt 8), beruht die Verarbeitung auf Art. 6 Abs. 1 lit. f DSGVO (berechtigtes Interesse an der Bearbeitung von Anfragen) bzw. Art. 6 Abs. 1 lit. b DSGVO, wenn die Anfrage einen Vertrag betrifft. Die Verarbeitung Ihrer Lesezeichen durch Ihren eigenen Server erfolgt in Ihrer eigenen Verantwortung bzw. der des Serverbetreibers.

## 8. Kontakt per E-Mail

Wenn Sie der verantwortlichen Stelle per E-Mail schreiben, werden Ihre Nachricht und Ihre Adresse zur Bearbeitung Ihres Anliegens gespeichert und danach gelöscht, sofern keine gesetzlichen Aufbewahrungsfristen entgegenstehen.

## 9. Ihre Rechte

Sie haben das Recht auf Auskunft, Berichtigung, Löschung, Einschränkung der Verarbeitung, Datenübertragbarkeit und Widerspruch (Art. 15 bis 21 DSGVO) sowie auf jederzeitigen Widerruf einer erteilten Einwilligung. Für Daten auf Ihrem Server sind Sie selbst Betreiber (oder fragen den Betreiber). Alle bookmoor-Daten im Browser löschen Sie, indem Sie die Erweiterung entfernen, die auf dem Server, indem Sie dessen State-Datei löschen. Außerdem haben Sie ein Beschwerderecht bei der zuständigen Datenschutz-Aufsichtsbehörde.

## 10. Sicherheit

Nutzen Sie für die Serveradresse HTTPS und legen Sie Tokens mit dem kleinsten nötigen Scope an (`sync` für einen Browser, `admin` nur für die Administration). Das Token wird im lokalen Speicher der Erweiterung abgelegt und auf der Einstellungsseite nicht vollständig angezeigt.

## 11. Änderungen

Ändert sich die Software so, dass diese Erklärung betroffen ist, wird diese Seite aktualisiert und das Datum oben angepasst. Frühere Fassungen stehen in der Git-Historie dieses Repositorys.
