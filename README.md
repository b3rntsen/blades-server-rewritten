# Blades Rewritten Server (Local Hosted)

This is a re‑implementation of the Blades server, as documented via reverse‑engineering (both binary analysis and packet capture).

## How to run:

1. Install Server packages:
   sudo apt install cargo rustup diesel-cli

4. Configuring the database:
   - Create a `.env` file in the server root folder with the `DATABASE_URL` being the PostgreSQL connection string, as expected by diesel.
   - Run `diesel migration run` to apply the database migrations.

5. Extracting game data (generate the `data` folder):
   - Use the unity asset ripper to extract the game data from the APK
   - Execute `scripts/data_parser/main.py`, setting the output file to `data/parsed.json`
   - Execute `scripts/generate_download_from_dump.py` (after fixing the path) with a capture that downloaded the full game (so the client can download it)

6. Configure mitmproxy:
   - Run `mitmweb --mode reverse:http://127.0.0.1:8000 --listen-host <Your machine IP or domain> --listen-port 443 --certs <Your machine IP or domain>=leaf-combined.pem -s mitmproxy_script.py

   (DEPRECATED: --set tls_version_client_min=UNBOUNDED` (adapt port as needed. You can use `--set web_port=...`). This will redirect HTTP request to port 8000.)

7. Build a patched APK that trust user‑installed certs:
   - Install dependencies (sudo apt install apktool)
   - generate an APK from the app and copy it to `build-app/source-package.apk`.
   - Run `build_patched_apk.sh`
   - Install the generated APK

8. Run the server:
   - Run `ARENA_DEV_LOGIN_USER_ID=<Your in-game User ID, taken from server traffic mitm> ARENA_IMPORT_TOKEN=<make it up> cargo run ->

   (DEPRECATED: --enet-listen-addr 127.0.0.1:8001 --enet-public-addr <machine network/public IP>:8001`)

## Some SQL notes:
Remember to use FOR NO KEY UPDATE in your select if you’re gonna write back the modified result (obviously in the same transaction). Take care of deadlock too! (the for FOR NO KEY UPDATE should handle that in most cases).

## Licence

AGPL-3.0-only — see [LICENSE](LICENSE).

This is a fork of [marius851000/blades-server-rewritten](https://github.com/marius851000/blades-server-rewritten),
which is MIT. Those portions remain available under MIT from their original
author and upstream is unaffected; see [NOTICE](NOTICE).

The AGPL was chosen because this is a server: under permissive terms a modified
copy can be run as a public service without publishing the changes, and section
13 closes that. For a preservation project that is the point.

Some files carry game data this project did not author and cannot license to
you. **Read [NOTICE](NOTICE) before redistributing** — it says exactly what
that data is and, as importantly, what is not in this repository.
