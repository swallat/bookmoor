#!/usr/bin/env node
// Packs a zip of the extension into a signed CRX3 file, as needed for the Chrome
// Web Store "Verified CRX uploads" (see docs/development.md). No dependencies.
//
//   node scripts/pack-crx.mjs <in.zip> <private-key.pem> <out.crx>
//
// Format: "Cr24", u32le 3, u32le header length, CrxFileHeader (protobuf), zip.
// The signature (RSA PKCS#1 v1.5, SHA-256) covers "CRX3 SignedData\0", the
// length of the signed header data, that data (the crx_id) and the zip.
import { createHash, createPrivateKey, createPublicKey, createSign } from "node:crypto";
import { readFileSync, writeFileSync } from "node:fs";

const [zipPath, keyPath, outPath] = process.argv.slice(2);
if (!zipPath || !keyPath || !outPath) {
  console.error("usage: pack-crx.mjs <in.zip> <private-key.pem> <out.crx>");
  process.exit(2);
}

const varint = (n) => {
  const out = [];
  while (n > 0x7f) {
    out.push((n & 0x7f) | 0x80);
    n = Math.floor(n / 128);
  }
  out.push(n);
  return Buffer.from(out);
};
// Protobuf length-delimited field.
const field = (num, bytes) => Buffer.concat([varint((num << 3) | 2), varint(bytes.length), bytes]);
const u32 = (n) => {
  const b = Buffer.alloc(4);
  b.writeUInt32LE(n);
  return b;
};

const zip = readFileSync(zipPath);
const privateKey = createPrivateKey(readFileSync(keyPath));
if (privateKey.asymmetricKeyType !== "rsa") throw new Error("the key must be an RSA key");
const publicKey = createPublicKey(privateKey).export({ type: "spki", format: "der" });

const crxId = createHash("sha256").update(publicKey).digest().subarray(0, 16);
const signedHeaderData = field(1, crxId); // SignedData { crx_id = 1 }

const signer = createSign("sha256");
signer.update("CRX3 SignedData\0");
signer.update(u32(signedHeaderData.length));
signer.update(signedHeaderData);
signer.update(zip);
const signature = signer.sign(privateKey);

const keyProof = Buffer.concat([field(1, publicKey), field(2, signature)]);
// CrxFileHeader { sha256_with_rsa = 2, signed_header_data = 10000 }
const header = Buffer.concat([field(2, keyProof), field(10000, signedHeaderData)]);

writeFileSync(outPath, Buffer.concat([Buffer.from("Cr24"), u32(3), u32(header.length), header, zip]));
console.log(`wrote ${outPath} (${header.length + zip.length + 12} bytes, key id ${crxId.toString("hex")})`);
