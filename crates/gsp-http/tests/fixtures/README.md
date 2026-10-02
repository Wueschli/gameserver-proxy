# Test-only TLS fixtures — never use outside tests

A private CA (`ca.pem`), a `localhost` / `127.0.0.1` server certificate it signed
(`leaf.pem`, key `leaf.key`), and an unrelated CA (`other-ca.pem`). Valid until 2126.
The CA private keys were discarded after signing; `leaf.key` is committed on purpose
so tests can run a TLS server. Used by `crates/gsp-http/tests/` and
`crates/gsp-fleet-tests` (`--ca-file` tests).

Regenerate (all four files together):

```sh
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -days 36500 \
  -subj "/CN=gsp-http test CA" -keyout ca.key -out ca.pem
openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -subj "/CN=localhost" \
  -keyout leaf.key -out leaf.csr
printf "subjectAltName=DNS:localhost,IP:127.0.0.1\nbasicConstraints=CA:FALSE\nkeyUsage=digitalSignature\nextendedKeyUsage=serverAuth\n" > ext.cnf
openssl x509 -req -in leaf.csr -CA ca.pem -CAkey ca.key -CAcreateserial -days 36500 \
  -extfile ext.cnf -out leaf.pem
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -days 36500 \
  -subj "/CN=gsp-http unrelated CA" -keyout other.key -out other-ca.pem
rm -f leaf.csr ext.cnf ca.key other.key ca.srl
```
