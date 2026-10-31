# network_comms

Part of the [telemetry review](../detectors.md#what-the-telemetry-says) of the [preseed detectors](../detectors.md).

**Status (2026-10-31):** the fixes below shipped in the fleet-wide detector overhaul (see the main page's overhaul status note). No deviations from the proposals below. Additions: indirect import uses (mov-reg-then-call) count as call-site evidence when the site sits in a function (VMProtect import slots still drop); glued known-header templates (Content-Type:…) classify again; the unseeded-IOC digest lists IPs/URLs/endpoints ahead of fabric so the line cap never drops an address.

network_comms is the fleet's highest-volume detector and the only C2-tactic seeder. Its useful leads are genuinely load-bearing — killswitch and C2 URLs, beacon request fabric (User-Agent, multipart Content-Type, HTTP/1.x templates), and real WinINet/socket anchors are the backbone of the C2 question — but its false positives are dominated by a few mechanical sub-passes: the folded sin_port recovery that byte-swaps ASCII character stores into fake "non-standard ports", shape-only string classifiers that read library documentation URLs, VERSIONINFO quads, filesystem paths, and Rust/HTTP-stack log prose as IOCs, and rollup exemptions that keep all of that seeded (the NetworkEndpoint pivot-artifact veto for the URL/IP classes, and the never-suppressed file-level bucket plus unnamed-FUN_* attribution for the fabric classes). Because findings share one thread per function, each junk sub-finding (a fake port, a doc URL) also poisons the verdicts of correct API-call leads in the same bucket. The ignore mass is capability wallpaper: one lead per calling function per socket API, up to ~212 capped IOC-string leads per sample multiplied across multi-sample sessions, plus pool posts and rollup lines that re-list already-seeded material.

## Where the false positives come from
---

### 1. Folded port char store

The folded-htons recovery treats ANY 16-bit immediate store (`MOV word ptr [..], imm`) inside any function that already has a networking hit as a candidate sockaddr_in.sin_port, byte-swaps it, and accepts the result if it is >=1024 or a standard port. Stack-built strings and small struct constants dominate real binaries: a single-byte immediate (one ASCII/UTF-16 char, a size field, STARTUPINFO.wShowWindow) swaps to a multiple of 256 >=1024, and two-letter string fragments swap to plausible ports. For example, 'G'/'E'/'T' wide chars swap to 18176/17664/21504, two-letter fragments of stack-built service or API names ("rs", "nW", "oW", "le") to 29299/28247/28503/27749, packet-framing sizes 20..92 to 5120/23552, and wShowWindow=5 to 1280. The network_funcs gate is too broad: it is built from every prior hit's function (all five API catalogs plus ordinal and symbol hits), so any WinINet/WinHTTP caller qualifies — and WinINet code never builds sockaddrs but does build API-name strings on the stack. These leads are medium severity (unsure/40), so they draw engagement — and their byte-level disproofs then count against sibling leads in the same per-function thread.

*Where:* `thorpal/src/thorpal/detect/detections/command_and_control/network_comms.rs:2155-2171 (folded_port), 2277-2306 (folded sub-pass), 2406 (network_funcs gate)`

### 2. Library doc URL noise

Documentation/panic-message URLs shipped by statically linked runtimes (getrandom, gethostname.rs, hyper) are classified as Network IOC URLs. The fragment demotion already in the code (has_fragment -> low severity at line 1190, 'Reported for completeness' wording) does not stop the lead from seeding: the hit carries an ArtifactClass::NetworkEndpoint artifact, which exempts it from both the runtime-noise classification (detections.rs:931-944) and bucket suppression (seed.rs:466-482), and file-level buckets are never suppressed at all (seed.rs:488-489). Attribution makes it worse: the carve sweep's three seam rules (nested scheme, web extension + lowercase, colon segment) miss a plain-path continuation — '#' is a legal URL byte in is_url_byte — so 'https://docs.rs/getrandom#nodejs-es-module-support' + the adjacent literal '/dev/urandom' merge into one fake URL, and the DATA_REF_WINDOW=4096 buffer fallback attributes it to whatever function references nearby data (getrandom's Error::fmt — an unnamed FUN_* that is_runtime_function and the toolchain markers cannot recognize). Ransom-note URLs copied by C++ static initializers get the same T1071 'Web Protocols' claim in binaries with zero network APIs.

*Where:* `thorpal/src/thorpal/detect/detections/command_and_control/network_comms.rs:1185-1210 (severity), 1964-2045 (ioc_hit still seeds), 54+1564-1575 (DATA_REF_WINDOW fallback), 1615-1752 (carve seams; '#' is a legal URL byte); seed.rs:452-497 (protected-artifact veto, file-level never suppressed)`

### 3. Version string ipv4

parse_ipv4_literal accepts any no-leading-zero dotted quad except 0.x/broadcast, so PE VERSIONINFO FileVersion/ProductVersion values (such as '1.0.0.0' — UTF-16 strings in .rsrc) classify as IP-address IOCs. ioc_records walks ctx.strings with no section filter and no xref requirement; an unreferenced IP only drops to low severity (network_comms.rs:1987-1991) but still seeds a finding, and its NetworkEndpoint artifact + file-level bucket keep it out of the rollup. The flag text even admits the ambiguity ('statically indistinguishable from a version number') yet asks agents to go disprove it, including in binaries with zero network imports.

*Where:* `thorpal/src/thorpal/detect/indicators.rs:313-337 (parse_ipv4_literal); network_comms.rs:1541-1588 (no section/xref gate in ioc_records), 1987-1991 (unreferenced -> low but still seeded)`

### 4. Junk uri path

is_uri_path's exclusion lists are incomplete, so three families of leading-slash literals classify as URI paths: (1) dot-directory filesystem paths — '/.libs/' (gnulib's libtool-wrapper prefix strip) is not caught because FILESYSTEM_PREFIXES has no entry for dot-dirs; (2) macOS framework paths — '/System/Library/Frameworks/...' (only Unix prefixes /bin/../var/ are listed, no /System/, /Library/); (3) base64 ciphertext whose first byte happens to be '/' — an encrypted string table can yield '/'-leading base64 tokens because '=', '+', and mixed-case alnum are all in the legal path character set. These also feed the pool pass.

*Where:* `thorpal/src/thorpal/detect/indicators.rs:664-723 (is_uri_path), 418-421 (FILESYSTEM_PREFIXES)`

### 5. Prose substring fabric

classify_http matches on shape fragments that library prose satisfies: (a) any string containing 'http/1.0|1.1|2' anywhere becomes an 'HTTP Request Template' (indicators.rs:863-865) — hyper/h2 diagnostics like 'TE headers not set to "trailers" are illegal in HTTP/2 requests' and 'put; existing idle HTTP/2 connection for' are flagged as hand-built attacker templates; (b) split_header accepts 'Connection::poll; IO error' (a Rust path — after split_once(':') the name 'connection' is a standard header and the rest ':poll; IO error' is never inspected); (c) the k=v;+placeholder cookie rule (913-919) reads ODBC connection strings ('DRIVER=%s;SERVER=%s;UID=%s;PWD=%s...') as cookie material; (d) 'Date: %s' matches the header-template grammar with no gate on network use — it can just as well be an fprintf into a local report file; (e) bare Method tokens match dialog titles ('OPTIONS') and C2 response keywords ('DELETE'). In statically linked Rust binaries these sit in unnamed functions or have no resolved reference, so the runtime-code weakening never fires (is_runtime_function needs a recognizable name; FUN_* matches neither the CRT markers nor the toolchain markers).

*Where:* `thorpal/src/thorpal/detect/indicators.rs:863-865 (http/x substring), 762-775+868-881 (split_header), 913-919 (k=v; cookie), 846-921 (classify_http); network_comms.rs:1877-1880 (name-based is_runtime_function misses FUN_*); toolchain.rs:210 (is_toolchain_runtime, FUN_* pinned false by test)`

### 6. Dual use API overclaim and thread collateral

Two intertwined effects on the API-call leads. First, catalog reasoning asserts intent per call site ('candidate C2 endpoint', 'C2 beacon or exfiltration') for ubiquitous dual-use APIs, so benign-but-real callers get disputed: a license-activation time fetch (HttpSendRequest), the UDP-connect-to-8.8.8.8 getsockname local-IP trick (inet_addr), GDI+ JPEG-encoder CLSID lookup ('image/jpeg' Content-Type), an MS17-010 vulnerability probe (socket/send/recv). Second — the larger slice — seeding posts one thread per function with every finding as a comment (seed.rs:234-245), so an agent's disproof of ONE junk sub-finding (a fake folded port, a misread Sleep from a sibling detector, another detector's MAC-address claims) grades every co-located network lead as disputed, even when the same comment affirms the API lead as real network capability. This is partly a grading artifact, but the detector causes most of it by seeding junk into otherwise-correct buckets.

*Where:* `thorpal/src/thorpal/detect/detections/command_and_control/network_comms.rs:149-486 (catalog reasoning strings); seed.rs:234-245 (one post per function bucket, findings as comments)`

### 7. Pool junk amplification

ioc_pools groups whatever ioc_records produced with no member-quality gate — contiguous_pools buckets by class+adjacency only, shared_function_pools by attributed function only — so junk members become 'Indicator Pools' that imply a selection structure: three URLs that are offsets inside ONE contiguous ransom-note string literal (referenced by a C++ static initializer, zero network APIs in the binary) become a 3-member shared-function pool; statically linked OpenSSL asn_mime.c literals ('Content-Type: %smime;', 'multipart/signed', ...) pool under an unnamed library function with a 'back-edge' selection idiom; macOS framework dylib paths pool as 'URI Path @ contiguous data region'. Pools also re-post members already seeded individually, doubling the thread count per table.

*Where:* `thorpal/src/thorpal/detect/detections/command_and_control/ioc_pools.rs:298-358 (grouping with no member-quality gate), 481-554 (pool_hits)`

### 8. Endpoint shape coincidence

parse_endpoint/plausible_endpoint_host accept any dotted name with an alphabetic TLD plus :digits — and valid_url_host permits '_', which is illegal in DNS hostnames — so 'ISO_646.basic:1983' (an entry in a UTF-16 IANA charset-name table; 1983 is the standard's year) becomes a 'candidate C2 endpoint on a non-standard port', attributed to an unrelated function via the 4096-byte buffer-reference fallback. Separately, the detector trusts the defined-string table's boundaries verbatim (ioc_records reads string.value/string.address unverified), so a boundary off-by-one can drop the leading digit of an IP:port string and seed the wrong IOC value.

*Where:* `thorpal/src/thorpal/detect/indicators.rs:147-186 (valid_url_host allows '_', plausible_endpoint_host), 291-302 (parse_endpoint); network_comms.rs:1541-1551 (string value/address taken from the string table unverified)`

## Why leads get ignored
---

- **Per function API call wallpaper** — api_call_hits emits one lead per calling function per catalog API with no per-catalog cap, and the socket catalog includes pure plumbing at untrusted/15 (setsockopt, closesocket, select, poll, ioctlsocket, bind, WSAStartup, inet_addr). A networked binary therefore wallpapers the forum with dozens of capability restatements that answer no question an agent has; they engage the function once (if at all) and ignore the rest.
- **Static stack request fabric flood** — A statically linked HTTP/TLS stack ships every standard header, method, media type, and version string as literals. The per-class caps (12 per HTTP class x 9 classes + 40 URLs + 40 endpoints + 24 IPs = up to 212 IOC-string leads per sample, plus pools and ports) still flood, and the defenses all miss static libraries: the Method/ContentType/HeaderName drop and the weaken-one-step key on is_runtime_function, which needs a recognizable NAME — stripped static library code is FUN_*. The rollup cannot absorb them either: URL/endpoint/IP hits carry the pivot-protected NetworkEndpoint artifact (seed.rs:466-482 vetoes suppression), file-level buckets are never suppressed at all (seed.rs:488-489), and medium fabric hits in FUN_* functions never classify as Runtime context, so partition_suppressed's is_noise test keeps their buckets loud.
- **Multi sample session multiplication** — Sessions can carry multiple scoped samples, and seeding runs per sample, so the per-sample caps multiply and the ignored mass concentrates in those sessions.
- **Rollup lines graded as leads** — The runtime-noise rollup post itemizes each suppressed hit as a one-line entry ('- T1095 @ 0x...: WSAStartup call @ 0x...'). These lines are records by design, not questions, and agents correctly never engage them — but the grader counts each as a lead, adding structural ignores.
- **Pool posts duplicate members** — Every pool re-lists indicators that were already seeded as individual findings (pool_hits draws members from the same uncapped record set the per-class flags come from), so a hosts table generates its member leads plus up to 12 pool leads repeating them; agents read the members once and skip the restatement.

## What would reduce the noise
---

Static-side changes, ordered as proposed; each was checked against the detector's true positives so the fix does not suppress what the detector gets right.

### Reject character-store artifacts in folded sin_port recovery

In folded_port (network_comms.rs:2155-2171): reject immediates < 0x100 (single-byte stores byte-swap to XX00 'ports') and immediates whose both bytes are ASCII alphanumeric (string-fragment stores: 0x7372 'rs', 0x576e 'nW', 0x576f 'oW', 0x656c 'le'). In port_hits, narrow the folded sub-pass gate from the all-hit network_funcs set (built at network_comms.rs:2406) to functions carrying T1095 raw-socket hits (filter hits by technique id before building the set), since WinINet/WinHTTP callers never build sockaddrs but do build API-name strings on the stack; optionally also require an AF_INET marker (a word store of 0x0002) in the same function.

*Effort:* small \
*True-positive risk:* A real folded 443/80 (imm 0xBB01/0x5000) survives both immediate rules; a real folded port whose network-order bytes are both alphanumeric (e.g. 31337 stores imm 0x697A 'iz') in a raw-socket function would be dropped — the htons-argument sub-pass still catches unfolded builds, and the alnum rule can be limited to non-raw-socket functions if that trade is unwanted.

Addresses: folded port char store, dual use api overclaim and thread collateral.

### Stop seeding fragment URLs as findings and terminate carves at '#'

In ioc_hit (network_comms.rs:1964), return None for NetIoc::Url where url.has_fragment(); in NetworkComms::run, filter fragment-URL records out of the set passed to ioc_pools::pool_hits (one-line filter on records), and list their values in the per-sample unreferenced-strings digest from the cap/digest fix so they stay visible. In carve_url/is_url_byte (network_comms.rs:1615-1621), treat '#' as a run terminator: a fragment is never transmitted, so carving through it only merges the doc link with the next blob literal ('#nodejs-es-module-support/dev/urandom').

*Effort:* small \
*True-positive risk:* A fragment-bearing C2 URL is self-contradictory (the detector's own has_fragment reasoning says the fragment never hits the wire, RFC 3986 §3.5). Risk is effectively zero; the host still appears in the digest for corroboration.

Addresses: library doc url noise, static stack request fabric flood.

### Gate bare-IP findings on code use and resource-section location

In ioc_string_hits/ioc_hit (pass network_funcs in from NetworkComms::run, where it is already computed before ioc_records runs): seed a NetIoc::Ip finding only when the string has >=1 xref whose containing function resolves (a true code reference, not a data pointer) AND that function also carries a networking API/symbol hit; route the rest into the unreferenced-strings digest instead of per-lead findings. Additionally skip strings whose address falls in a resource section (section name from ctx.initialized_bytes()) or whose neighboring defined strings are VERSIONINFO keys (FileVersion/ProductVersion/VS_VERSION_INFO).

*Effort:* medium \
*True-positive risk:* An IP such as '127.0.0.1' referenced from a function that also calls WSAStartup passes the xref+network-function gate. A hardcoded C2 IP in dead/encrypted config data would drop to the digest rather than a lead — it remains visible verbatim, and the digest fallback bounds the damage from any xref-less true positive.

Addresses: version string ipv4.

### Tighten classify_http grammars against library prose

In indicators.rs classify_http: (a) line 863-865 — require the http/1.x|2 token to be word-bounded AND the string to start with an HTTP method, end with the version token, or contain CRLF (kills mid-sentence diagnostics like 'TE headers not set to...' and 'ALPN upgraded to HTTP/2', keeps the bare 'HTTP/1.0'/'HTTP/1.1' and real request lines); (b) split_header callers (868-881) — reject when the byte after the colon is another ':' (Rust paths like 'Connection::poll') and require the rest to be empty, start with whitespace, or carry a placeholder; (c) drop the k=v;+placeholder cookie rule (913-919; its catch is ODBC/connection strings) or demote its output to the digest; (d) demote bare Method tokens (rule 6, line 906) to digest-only.

*Effort:* medium \
*True-positive risk:* Real request fabric survives: 'HTTP/1.0'/'HTTP/1.1' templates end with the version token — kept; 'Host: ...' headers (space after colon) — kept; 'Content-Type: multipart/form-data; boundary=...' — kept; multi-line 'Date: %s\r\nConnection: keep-alive...' templates take the split_header path with whitespace after the colon — kept. Residual: a hand-rolled header stored with no space after the colon would be missed — rare, and the request-template path usually also fires for such samples.

Addresses: prose substring fabric, static stack request fabric flood.

### Extend is_uri_path rejection lists

In indicators.rs (FILESYSTEM_PREFIXES at 418-421, is_uri_path at 664-723): add '/System/', '/Library/', '/Applications/', '/Volumes/' to FILESYSTEM_PREFIXES; reject paths containing a dot-directory segment ('/.'); reject single-segment paths with no '?' that end in '=' or are base64-shaped (len >= 12, mixed-case alphanumerics with no further '/'). Route rejects that still look path-like into the digest rather than silently dropping.

*Effort:* small \
*True-positive risk:* A multi-segment beacon path with a web extension (e.g. '/ext/index2.php') survives every added rule. Residual: a genuine single-segment base64-ish beacon path (some C2 frameworks use them) could be demoted — hence demote-to-digest rather than drop, and keep anything with a '?' or a known web extension.

Addresses: junk uri path.

### Require member substance before emitting an indicator pool

In ioc_pools::pool_hits (and the two grouping fns at 298-358): exclude fragment-URL and runtime-weakened records from pool membership (do NOT exclude low()-severity members — see tp_risk); require each pool to retain >=1 member that has a code xref or medium+ severity; require contiguous pools' members to have >=1 code xref or an attributed non-library function; suppress pools whose members are exclusively HTTP-fabric classes unless the shared referencing function also carries a networking API/symbol hit; and skip a 'pool' whose members' string addresses all fall inside the bounds of one defined string from ctx.strings (the ransom-note case: adjacent carve offsets within a single literal).

*Effort:* medium \
*True-positive risk:* MIN_POOL_MEMBERS is 3 (ioc_pools.rs:52), so excluding low()-severity members would dissolve a real 3-member pool in which one member (e.g. a Content-Type) is low severity — so that exclusion was dropped from this fix; with membership kept intact, a pool whose shared function carries WinINet hits passes the fabric gate and onion-URL pools pass the xref gate (members referenced from a function that also calls InternetConnect). Pools are corroborating structure, not primary evidence, so the residual risk is the smallest of the set.

Addresses: pool junk amplification, pool posts duplicate members.

### Collapse low-severity socket/TLS plumbing into one capability lead per function

After the api_call_hits/ordinal_hits calls in NetworkComms::run, merge all low()-severity SOCKET_APIS/TLS/WEB setup hits (WSAStartup, socket, WSASocket, bind, setsockopt, ioctlsocket, select, poll, closesocket, inet_addr/inet_pton, InternetOpen, WinHttpOpen, curl_global_init/curl_easy_init, SSL_CTX_new/SSL_new/TLS_client_method, AcquireCredentialsHandle) that share a hit.function into a single 'network capability' finding listing the APIs and sites, keeping per-API leads only for the medium traffic-moving calls (connect/send/recv/sendto/HttpSendRequest/...). Reword the ubiquitous-API catalog reasonings (network_comms.rs:149-486) from asserted intent ('C2 beacon or exfiltration', 'candidate C2 endpoint') to capability language, reserving intent claims for corroborated findings.

*Effort:* medium \
*True-positive risk:* Cheap API leads (InternetOpen, WSAStartup, socket) serve as anchors on the function, not on the specific API — a merged per-function capability lead preserves the anchor, every API name, and every address; nothing is dropped, only merged.

Addresses: per function api call wallpaper, dual use api overclaim and thread collateral.

### Let the rollup/digest absorb unreferenced and untrusted IOC strings (and trim caps)

Attach the pivot-class NetworkEndpoint artifact only when an IOC hit has a code xref or >=medium severity, so partition_suppressed (seed.rs:452-497) can fold untrusted endpoint-class buckets — note only NetworkEndpoint is pivot-protected (detections.rs:934-941, seed.rs:470-479); HttpHeader/UserAgent/UriPath artifacts never veto, so the fabric flood needs the second half: add a per-sample 'unreferenced network strings' digest post for file-level/unreferenced untrusted IOC strings (file-level buckets bypass partition_suppressed entirely per seed.rs:488-489, so this must be a new digest at seeding time, listing each value verbatim like the existing rollup). Simultaneously trim the per-class caps at network_comms.rs:64-78 (MAX_HTTP_CLASS_HITS 12 -> 6, MAX_URL_HITS 40 -> 16, MAX_ENDPOINT_HITS 40 -> 16, MAX_IP_HITS 24 -> 8).

*Effort:* large \
*True-positive risk:* Highest-risk fix of the set: a useful hardcoded 'Host:' header can be file-level, unreferenced, and low severity, so it fails the 'xref OR >= medium' gate and survives only through the digest — the digest must therefore list every folded value verbatim so agents can still pivot (agents do read seeded values). A killswitch URL with code xrefs keeps its artifact. Cap-trimming risk is low.

Addresses: static stack request fabric flood, multi sample session multiplication, library doc url noise, version string ipv4.

## Verification notes
---

Each mechanism, anchor, and fix was re-checked against the code:

- All eight mechanism code anchors were verified line-exact (network_comms.rs 2155-2171/2277-2306/2406/1185-1210/1964-2045/1541-1588/1615-1752/1877-1880/54/1551; indicators.rs 165-186/291-302/313-337/418-421/664-723/762-775/846-921/863-865/913-919; ioc_pools.rs 298-358/481-554; seed.rs 234-245/452-497/489; detections.rs 931-944) and each mechanism's logic reproduces: folded_port accepts any 16-bit immediate store and byte-swaps (0x47->18176, 0x7372->29299, 0x0005->1280); network_funcs at :2406 collects every prior hit's function including WinINet/WinHTTP callers; classify_http reproduces each misclassification ('http/2' substring -> RequestTemplate, 'Connection::poll' -> HeaderName, ODBC k=v;%s -> CookieName, 'Date: %s' -> HeaderTemplate, 'OPTIONS' -> Method); is_uri_path accepts '/.libs/', '/System/...', and '/'-leading base64 tokens; plausible_endpoint_host accepts 'ISO_646.basic' (underscore legal via valid_url_host:153).
- REJECTED one element of the pool fix ('Require member substance before emitting an indicator pool'): excluding low()-severity members from pool membership would dissolve real 3-member pools (MIN_POOL_MEMBERS is 3, ioc_pools.rs:52) that contain a low-severity member such as a Content-Type. The member gate excludes only fragment-URL/runtime-weakened members and requires >=1 substantial member instead.
- CORRECTED an overreaching artifact-veto claim (in the static-stack-request-fabric-flood driver and the rollup/digest fix): only ArtifactClass::NetworkEndpoint (plus config/key/cert/host-id classes) is pivot-protected in detections.rs:931-944 and seed.rs:466-482; HttpHeader/UserAgent/UriPath artifact classes are NOT protected. The fabric flood bypasses the rollup via the never-suppressed file-level bucket (seed.rs:488-489) and via is_noise requiring CallContext::Runtime for unsure hits, which FUN_* names never get (is_library_function matches CRT markers only; is_toolchain_runtime pins FUN_* false in its own test, toolchain.rs:345). The rollup/digest fix gates only NetworkEndpoint, and its file-level handling is a new digest post rather than a partition_suppressed change (file-level buckets never reach that gate).
- The per-sample IOC-string cap math is 212 (12x9 fabric + 40 URL + 40 endpoint + 24 IP).
