# fcrackzip (Unzip)

fcrackzip is a fast password cracker for ZIP archives. In this image it is wrapped to detect encrypted ZIP files, recover their password via a dictionary attack, and extract the archive contents for further analysis.

## Overview

fcrackzip cracks password-protected ZIP archives using brute-force and dictionary attacks, verifying candidate passwords against the archive (its `--use-unzip` mode confirms a correct guess). This deployment first uses 7-Zip to check whether a ZIP uses ZipCrypto encryption; if so, it runs fcrackzip with a bundled dictionary to find the password and then unzips the contents, otherwise it extracts the archive directly. Recovered files are emitted as child artifacts and encrypted archives are tagged `Encrypted: True`. The bundled dictionary is small and oriented toward common malware-sample passwords (e.g. "infected", "malware", "password").

## Supported File Types

- ZIP archives (encrypted and unencrypted)

## Usage

Run this tool on ZIP archives to unpack their contents for downstream analysis, and specifically when a sample is a password-protected ZIP. It is well suited to malware samples distributed as encrypted ZIPs that use common, well-known passwords, since the bundled dictionary targets those conventions. Note that password recovery is limited to the dictionary provided in this image, so archives with strong or unusual passwords will not be cracked.

## Documentation

[fcrackzip on Debian Salsa](https://salsa.debian.org/pkg-security-team/fcrackzip)

## License

GPL-2.0 (fcrackzip); see [COPYING](https://github.com/hyc/fcrackzip/blob/master/COPYING). This image also bundles p7zip (LGPL).
