# Unzip

Extracts the contents of ZIP archives, including password-protected ones, by detecting encryption and attempting password recovery with a bundled dictionary before unpacking. Recovered files are emitted as child artifacts for downstream analysis.

## Images Run

- **unzip** — Runs the fcrackzip image, which detects ZipCrypto-encrypted ZIPs, recovers their password via a dictionary attack, and extracts the archive contents (encrypted archives are tagged and recovered files emitted as children).

## Supported File Types

- ZIP archives (encrypted and unencrypted)

This pipeline is triggered specifically on samples tagged with the `ZIP` file type.

## Usage

Run this pipeline on ZIP archives to unpack their contents for further analysis, and especially when a sample is a password-protected ZIP. It is well suited to malware samples distributed as encrypted ZIPs that use common, well-known passwords, since the bundled dictionary targets those conventions (e.g. "infected", "malware", "password"). Note that password recovery is limited to the dictionary provided in the image, so archives with strong or unusual passwords will not be cracked.
