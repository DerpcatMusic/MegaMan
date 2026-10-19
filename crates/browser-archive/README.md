# ZIP services

Worker-owned ZIP catalogs use typed member/prefix identities, bounded metadata/name/record budgets and explicit unsafe/unsupported entries. Browsing does not inflate payloads. Reads verify integrity/authentication within resource budgets.

Linux extraction and creation use private staging and new-destination publication. Extraction jobs retain their original browser/session lease, exact selected record identities and terminal outcome until acknowledgement. Existing destinations are not overwritten. Password UI, additional formats, nested opening and preview/drag leases remain in development.

Independent ZIP interoperability checks require an installed `7z` and run explicitly:

```sh
cargo test --locked -p browser-archive --test zip_interop -- --ignored
```

The tests generate disposable owned fixtures with 7-Zip, read/extract AES-128/192/256 and legacy ZipCrypto with Stored/Deflate, and verify MegaMan-created AES-256 archives through 7-Zip. They also check missing/wrong passwords and a damaged AES authentication trailer without destination publication or staging residue. Unicode names, empty files/directories and binary payloads are included. ZipCrypto is legacy compatibility, not authenticated encryption. These fixtures do not qualify every ZIP variant or large archive.

Fixture passwords are public test constants; the helper passes those disposable values as subprocess arguments. Production archive services operate in process and do not spawn this helper or pass user passwords to a command line.
