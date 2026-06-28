# nimmesh on TestFlight (remote / over-the-air builds)

Once you're traveling, your phone isn't on the Mini's network, so USB/Wi-Fi dev installs can't
reach it — and free-signing builds expire after 7 days. **TestFlight** fixes both: builds install
and update **over the internet from anywhere**, and last **90 days**. The mesh + real-funds tests
behave identically to a dev build (real device, real entitlements).

This needs the **$99/yr Apple Developer Program** (the free tier can't do over-the-air delivery).

## One-time setup (your part)

1. **Enroll** at <https://developer.apple.com/programs/> ($99/yr). Approval can take minutes to a
   day (identity verification).
2. **Create an App Store Connect API key** (so the Mini can upload headlessly — no 2FA prompts):
   App Store Connect → **Users and Access → Integrations → App Store Connect API → generate key**,
   Access = **App Manager**. Download `AuthKey_XXXXXXXXXX.p8` (you only get one download).
3. **Place the key + config on the Mini:**
   ```bash
   mkdir -p ~/.appstoreconnect/private_keys
   mv ~/Downloads/AuthKey_*.p8 ~/.appstoreconnect/private_keys/
   # then create ~/secrets/testflight.env (chmod 600):
   #   ASC_TEAM_ID=ABCDE12345
   #   ASC_KEY_ID=XXXXXXXXXX
   #   ASC_ISSUER_ID=xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx
   chmod 600 ~/secrets/testflight.env
   ```
   - `ASC_TEAM_ID` = your **paid** Team ID (Apple Developer → Membership details).
   - `ASC_KEY_ID` = the API Key ID (must match `AuthKey_<KEY_ID>.p8`).
   - `ASC_ISSUER_ID` = the Issuer ID shown above the keys list.
4. **Create the app record once:** App Store Connect → **+ New App** → iOS, name `nimmesh`,
   bundle id `com.nimmesh.app`, SKU `nimmesh`.
5. Hand me (or confirm) the Team ID / Key ID / Issuer ID and I'll wire signing to the paid team.

## The loop (after setup)

From the Mini (I run this for you):
```bash
apple/scripts/build-testflight.sh             # framework + archive + upload
apple/scripts/build-testflight.sh --skip-rust # faster when only Swift/webui changed
apple/scripts/build-testflight.sh --no-upload # build the signed .ipa only (dry run)
```
The script: regenerates the framework + project, archives Release for device with App Store
distribution signing (paid team, automatic), exports the `.ipa`, then validates + uploads via the
API key. Marketing version comes from `Cargo.toml`; the build number is a timestamp (always
unique). After a few minutes of processing it appears in **TestFlight**.

On your phone (one time): install the **TestFlight** app, accept the invite (you're added as an
**Internal Tester**). After that, each new build: open TestFlight → **Update**. From anywhere.

## Notes
- **Export compliance:** `ITSAppUsesNonExemptEncryption = false` is set in `project.yml` (the app
  uses only exempt crypto — Ed25519 signatures + TLS). Confirm that classification is right for you.
- **Two-phone offline-mesh test:** TestFlight gets the app onto both phones, but the Bluetooth
  test still needs **two iOS-16+ phones in the same room** (`docs/DEVICE-TEST.md`).
- **Dev builds still work** too (USB/Wi-Fi, free team) when you're home; TestFlight is for remote.
