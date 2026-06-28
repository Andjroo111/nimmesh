import Foundation

// C1e: verify Mnemonic.swift against the OFFICIAL BIP39 (Trezor) and SLIP-0010 ed25519 test
// vectors. Compile together with the real module so we test the shipping code, not a copy.
// swiftc requires the top-level-code file to be named `main.swift`, so copy this one first:
//   cp apple/scripts/verify-mnemonic-main.swift /tmp/main.swift
//   swiftc apple/NimmeshApp/Sources/Mnemonic.swift /tmp/main.swift -o /tmp/verify-mnemonic
//   /tmp/verify-mnemonic apple/NimmeshApp/Resources/bip39-english.txt
// Exit 0 = all vectors pass. The Nimiq-path (m/44'/242'/0'/0') + address correctness is the
// final user check: import your real wallet and confirm the derived NQ address matches.

func hex(_ d: Data) -> String { d.map { String(format: "%02x", $0) }.joined() }
func data(hex: String) -> Data {
    var out = Data(); var i = hex.startIndex
    while i < hex.endIndex {
        let j = hex.index(i, offsetBy: 2)
        out.append(UInt8(hex[i..<j], radix: 16)!); i = j
    }
    return out
}

var failures = 0
func check(_ name: String, _ got: String, _ want: String) {
    if got == want { print("  ✓ \(name)") }
    else { print("  ✗ \(name)\n      got:  \(got)\n      want: \(want)"); failures += 1 }
}
func checkBool(_ name: String, _ cond: Bool) {
    if cond { print("  ✓ \(name)") } else { print("  ✗ \(name)"); failures += 1 }
}

guard CommandLine.arguments.count > 1, let bip39 = Bip39(wordlistAt: CommandLine.arguments[1]) else {
    print("usage: verify-mnemonic <path-to-bip39-english.txt>  (wordlist must be 2048 words)")
    exit(2)
}
print("wordlist: 2048 words loaded\n")

// --- BIP39: official Trezor vectors (entropy -> mnemonic, mnemonic -> seed @ passphrase "TREZOR")
print("BIP39 (Trezor vectors):")
let bip39Vectors: [(ent: String, mnemonic: String, seed: String)] = [
    ("0000000000000000000000000000000000000000000000000000000000000000",
     "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon art",
     "bda85446c68413707090a52022edd26a1c9462295029f2e60cd7c4f2bbd3097170af7a4d73245cafa9c3cca8d561a7c3de6f5d4a10be8ed2a5e608d68f92fcc8"),
    ("7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f",
     "legal winner thank year wave sausage worth useful legal winner thank year wave sausage worth useful legal winner thank year wave sausage worth title",
     "bc09fca1804f7e69da93c2f2028eb238c227f2e9dda30cd63699232578480a4021b146ad717fbb7e451ce9eb835f43620bf5c514db0f8add49f5d121449d3e87"),
    ("ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
     "zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo vote",
     "dd48c104698c30cfe2b6142103248622fb7bb0ff692eebb00089b32d22484e1613912f0a5b694407be899ffd31ed3992c456cdf60f5d4564b8ba3f05a69890ad"),
]
for v in bip39Vectors {
    check("entropy->mnemonic", bip39.mnemonic(fromEntropy: data(hex: v.ent)) ?? "nil", v.mnemonic)
    check("mnemonic->entropy", hex(bip39.entropy(fromMnemonic: v.mnemonic) ?? Data()), v.ent)
    check("mnemonic->seed",    hex(bip39.seed(fromMnemonic: v.mnemonic, passphrase: "TREZOR")), v.seed)
}
checkBool("valid mnemonic accepted", bip39.isValid(bip39Vectors[0].mnemonic))
checkBool("tampered checksum rejected",
          !bip39.isValid("abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon"))
checkBool("off-list word rejected", !bip39.isValid("notaword " + bip39Vectors[0].mnemonic))

// --- SLIP-0010 ed25519: official spec vectors (seed -> hardened-path private key)
print("\nSLIP-0010 (ed25519 vectors):")
let s = data(hex: "000102030405060708090a0b0c0d0e0f")
func H(_ i: UInt32) -> UInt32 { i | 0x8000_0000 }
check("m",                        hex(Slip10.derive(path: [], seed: s)), "2b4be7f19ee27bbf30c667b642d5f4aa69fd169872f8fc3059c08ebae2eb19e7")
check("m/0'",                     hex(Slip10.derive(path: [H(0)], seed: s)), "68e0fe46dfb67e368c75379acec591dad19df3cde26e63b93a8e704f1dade7a3")
check("m/0'/1'/2'/2'/1000000000'",
      hex(Slip10.derive(path: [H(0), H(1), H(2), H(2), H(1_000_000_000)], seed: s)),
      "8f94d394a8e8fd6b1bc2f3f49f5c47e385281d5c17e65324b0f62483e37e8793")

// --- Nimiq HD: end-to-end determinism (the NQ-address correctness is the user import check)
print("\nNimiq HD (m/44'/242'/0'/0'):")
let phrase = bip39Vectors[0].mnemonic
let k1 = NimiqHD.privateKey(mnemonic: phrase, bip39: bip39)
let k2 = NimiqHD.privateKey(mnemonic: phrase.uppercased(), bip39: bip39)  // case-insensitive
checkBool("derives a 32-byte key", k1?.count == 32)
check("deterministic + case-insensitive", hex(k1 ?? Data()), hex(k2 ?? Data()))
checkBool("invalid phrase -> nil", NimiqHD.privateKey(mnemonic: "not a real phrase", bip39: bip39) == nil)

// --- generate() round-trips
print("\ngenerate() round-trip:")
let gen = bip39.generate()
checkBool("24 words", gen.split(separator: " ").count == 24)
checkBool("generated phrase is valid", bip39.isValid(gen))
checkBool("generated phrase derives a key", NimiqHD.privateKey(mnemonic: gen, bip39: bip39)?.count == 32)

print("")
if failures == 0 { print("ALL VECTORS PASS ✓"); exit(0) }
else { print("\(failures) FAILURE(S) ✗"); exit(1) }
