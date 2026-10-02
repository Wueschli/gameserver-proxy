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

Native-TLS rotation fixtures (`ca2.pem`, `leaf2.pem`, `leaf2.key`: a second CA and a
`localhost` leaf it signed; CA key discarded) and `leaf.sec1.key` (`leaf.key` in SEC1
`EC PRIVATE KEY` encoding):

```sh
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -days 36500 \
  -subj "/CN=gsp-http test CA 2" -keyout ca2.key -out ca2.pem
openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -subj "/CN=localhost" \
  -keyout leaf2.key -out leaf2.csr
# ext.cnf as above
openssl x509 -req -in leaf2.csr -CA ca2.pem -CAkey ca2.key -CAcreateserial -days 36500 \
  -extfile ext.cnf -out leaf2.pem
openssl ec -in leaf.key -out leaf.sec1.key
rm -f leaf2.csr ext.cnf ca2.key ca2.srl
```
