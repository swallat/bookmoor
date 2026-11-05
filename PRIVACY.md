# Privacy Policy

*[Deutsche Fassung: PRIVACY.de.md](PRIVACY.de.md) · Last updated: 5 November 2026*

This policy covers the bookmoor browser extension (Chrome and Firefox) and the bookmoor server software. It does not cover a website; the project page is the [GitHub repository](https://github.com/swallat/bookmoor).

## 1. Overview

- bookmoor syncs your bookmarks between your browsers through **a server that you run yourself** (or that someone you trust runs for you).
- **The developer receives no data.** There is no developer-operated server, account, analytics, advertising or tracking in the extension or the server.
- The extension sends data **only to the server address you enter** in its settings. It contacts no other site.
- bookmoor does nothing until you enter a server address and an API token.

## 2. Controller

For this software and this policy:

Sebastian Wallat - voidyapp.ing
c/o Autorenglück #23597
Albert-Einstein-Str. 47
02977 Hoyerswerda, Germany
E-mail: hello@voidyapp.ing

The **controller of the data on your bookmoor server is the operator of that server**. If you run it yourself, that is you. If someone else runs it for you, ask them about their data handling.

## 3. What the extension processes

| Data | Why | Where it goes |
| --- | --- | --- |
| Your bookmarks (titles, URLs, folder structure, order) | To sync them between your browsers | To your bookmoor server |
| The API token you enter | To authenticate with your server | To your bookmoor server (as `Authorization` header) |
| Extra headers and query parameters you enter (for example for a reverse proxy) | To get through your own proxy or share link | To your bookmoor server |
| Server address, token, extra headers, sync state (which local bookmark matches which server entry), last sync status | So the extension can work | Stored only in your browser (`storage`), on your device |

The extension asks for these browser permissions:

- `bookmarks`: read the bookmark tree and apply changes from other browsers.
- `storage`: keep the settings and the sync state described above.
- `alarms`: schedule uploads shortly after a change and regular checks for changes.
- Access to the server address you enter (requested as an optional host permission when you save the settings): to talk to your server, and only to it.

The extension includes no remote code and no third-party libraries that load at run time.

## 4. What the server processes

The server stores the bookmark tree it receives, a list of devices (endpoints), the hashed API tokens, and keeps a change log in a JSON file on the machine where it runs. It writes a request log (time, method, path, status, token name, and the client address as seen by the server) to its console output. Where that output goes and how long it is kept depends on how you run the server. The server sends nothing to third parties.

## 5. Recipients and third countries

bookmoor passes data to no one. Your bookmarks reach **only your own server**. If you put a reverse proxy, a tunnel or a hosting provider in front of it, those parties can see the traffic you route through them; choose them accordingly and use HTTPS.

Data transfers to third countries happen only if you run your server there.

## 6. Distribution channels

You may install the extension from the Chrome Web Store, from Mozilla Add-ons, or from a build from the GitHub repository. Those services (Google, Mozilla, GitHub) process their own data, for example download statistics or your IP address, under their own privacy policies. The developer has no access to your bookmoor data through them.

## 7. Legal basis (GDPR)

Where the developer is a controller (contact by e-mail, §8), processing is based on Art. 6(1)(f) GDPR (legitimate interest in answering inquiries) or Art. 6(1)(b) GDPR if the inquiry concerns a contract. The processing of your bookmarks by your own server takes place on your own responsibility or that of the server operator.

## 8. Contact by e-mail

If you write to the controller's e-mail address, your message and address are stored to handle your request and deleted when it is done, unless legal retention periods apply.

## 9. Your rights

You have the right to access, rectification, erasure, restriction of processing, data portability and objection (Art. 15 to 21 GDPR), and to withdraw consent given at any time. For data on your server, you are the operator (or ask the operator). You can delete all bookmoor data from your browser by removing the extension, and from the server by deleting its state file. You also have the right to lodge a complaint with the competent data protection supervisory authority.

## 10. Security

Use HTTPS for your server address, and create tokens with the least scope needed (`sync` for a browser, `admin` only for administration). The token is stored in the extension's local storage and is not shown in full on the settings page.

## 11. Changes

If the software changes in a way that affects this policy, this page is updated and the date above changes. Earlier versions are in the Git history of this repository.
