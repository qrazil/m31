#!/usr/bin/env python3
"""The other half of `t_chacha20poly1305.m31`: the same lines, computed
independently through `cryptography`/OpenSSL instead of `lib/crypto/chacha20poly1305.m31`.

`test.sh` diffs the two outputs. Nothing here reads the language's answer,
which is the whole point -- an oracle that has seen the result is a
tautology (the same rule `apps/git/oracle_sha1.py`'s header states).

    pip install cryptography   # 46.0.4 when this was written
    python3 apps/ssh/oracle_chacha20poly1305.py
"""
from cryptography.hazmat.primitives.ciphers import Cipher, algorithms
from cryptography.hazmat.primitives.poly1305 import Poly1305
from cryptography.hazmat.primitives.ciphers.aead import ChaCha20Poly1305
from cryptography.exceptions import InvalidTag

out = []


# --- the four primitives, each via `cryptography`'s own API ------------------

def chacha_block(key, counter, nonce):
    # `cryptography`'s `ChaCha20` takes one 16-octet nonce: a 4-octet
    # little-endian initial counter followed by the 12-octet IETF nonce
    # (RFC 8439's own layout) -- so the keystream for one counter value is
    # the cipher's output over a block of zeros, which is the block function
    # itself (§2.3: "XOR of this keystream with the plaintext", and zero
    # changes nothing).
    full_nonce = counter.to_bytes(4, "little") + nonce
    enc = Cipher(algorithms.ChaCha20(key, full_nonce), mode=None).encryptor()
    return enc.update(bytes(64))


def chacha_encrypt(key, counter, nonce, pt):
    full_nonce = counter.to_bytes(4, "little") + nonce
    enc = Cipher(algorithms.ChaCha20(key, full_nonce), mode=None).encryptor()
    return enc.update(pt)


def poly1305_key_gen(key, nonce):
    return chacha_block(key, 0, nonce)[:32]


def poly1305_tag(key, msg):
    return Poly1305.generate_tag(key, msg)


def aead_seal(key, nonce, pt, aad):
    return ChaCha20Poly1305(key).encrypt(nonce, pt, aad)


def aead_open_line(label, key, nonce, sealed, aad):
    try:
        pt = ChaCha20Poly1305(key).decrypt(nonce, sealed, aad)
        return f"{label} ok {pt.hex()}"
    except InvalidTag:
        return f"{label} err chacha20poly1305: authentication tag mismatch"


# --- RFC 8439 official test vectors -----------------------------------------
#
# Same inputs as t_chacha20poly1305.m31's RFC section -- see that file's
# header for why the expected numbers are not duplicated in either file.

out.append("block 2.3.2 " + chacha_block(bytes.fromhex("000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f"), 1, bytes.fromhex("000000090000004a00000000")).hex())
out.append("block A.1#1 " + chacha_block(bytes.fromhex("0000000000000000000000000000000000000000000000000000000000000000"), 0, bytes.fromhex("000000000000000000000000")).hex())
out.append("block A.1#2 " + chacha_block(bytes.fromhex("0000000000000000000000000000000000000000000000000000000000000000"), 1, bytes.fromhex("000000000000000000000000")).hex())
out.append("block A.1#3 " + chacha_block(bytes.fromhex("0000000000000000000000000000000000000000000000000000000000000001"), 1, bytes.fromhex("000000000000000000000000")).hex())
out.append("block A.1#4 " + chacha_block(bytes.fromhex("00ff000000000000000000000000000000000000000000000000000000000000"), 2, bytes.fromhex("000000000000000000000000")).hex())
out.append("block A.1#5 " + chacha_block(bytes.fromhex("0000000000000000000000000000000000000000000000000000000000000000"), 0, bytes.fromhex("000000000000000000000002")).hex())

out.append("encrypt 2.4.2 " + chacha_encrypt(bytes.fromhex("000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f"), 1, bytes.fromhex("000000000000004a00000000"), bytes.fromhex("4c616469657320616e642047656e746c656d656e206f662074686520636c617373206f66202739393a204966204920636f756c64206f6666657220796f75206f6e6c79206f6e652074697020666f7220746865206675747572652c2073756e73637265656e20776f756c642062652069742e")).hex())
out.append("encrypt A.2#2 " + chacha_encrypt(bytes.fromhex("0000000000000000000000000000000000000000000000000000000000000001"), 1, bytes.fromhex("000000000000000000000002"), bytes.fromhex("416e79207375626d697373696f6e20746f20746865204945544620696e74656e6465642062792074686520436f6e7472696275746f7220666f72207075626c69636174696f6e20617320616c6c206f722070617274206f6620616e204945544620496e7465726e65742d4472616674206f722052464320616e6420616e792073746174656d656e74206d6164652077697468696e2074686520636f6e74657874206f6620616e204945544620616374697669747920697320636f6e7369646572656420616e20224945544620436f6e747269627574696f6e222e20537563682073746174656d656e747320696e636c756465206f72616c2073746174656d656e747320696e20494554462073657373696f6e732c2061732077656c6c206173207772697474656e20616e6420656c656374726f6e696320636f6d6d756e69636174696f6e73206d61646520617420616e792074696d65206f7220706c6163652c207768696368206172652061646472657373656420746f")).hex())
out.append("encrypt A.2#3 " + chacha_encrypt(bytes.fromhex("1c9240a5eb55d38af333888604f6b5f0473917c1402b80099dca5cbc207075c0"), 42, bytes.fromhex("000000000000000000000002"), bytes.fromhex("2754776173206272696c6c69672c20616e642074686520736c6974687920746f7665730a446964206779726520616e642067696d626c6520696e2074686520776162653a0a416c6c206d696d737920776572652074686520626f726f676f7665732c0a416e6420746865206d6f6d65207261746873206f757467726162652e")).hex())

out.append("keygen 2.6.2 " + poly1305_key_gen(bytes.fromhex("808182838485868788898a8b8c8d8e8f909192939495969798999a9b9c9d9e9f"), bytes.fromhex("000000000001020304050607")).hex())
out.append("keygen A.4#1 " + poly1305_key_gen(bytes.fromhex("0000000000000000000000000000000000000000000000000000000000000000"), bytes.fromhex("000000000000000000000000")).hex())
out.append("keygen A.4#2 " + poly1305_key_gen(bytes.fromhex("0000000000000000000000000000000000000000000000000000000000000001"), bytes.fromhex("000000000000000000000002")).hex())
out.append("keygen A.4#3 " + poly1305_key_gen(bytes.fromhex("1c9240a5eb55d38af333888604f6b5f0473917c1402b80099dca5cbc207075c0"), bytes.fromhex("000000000000000000000002")).hex())

out.append("mac 2.5.2 " + poly1305_tag(bytes.fromhex("85d6be7857556d337f4452fe42d506a80103808afb0db2fd4abff6af4149f51b"), bytes.fromhex("43727970746f6772617068696320466f72756d2052657365617263682047726f7570")).hex())
out.append("mac A.3#1 " + poly1305_tag(bytes.fromhex("0000000000000000000000000000000000000000000000000000000000000000"), bytes.fromhex("00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000")).hex())
out.append("mac A.3#2 " + poly1305_tag(bytes.fromhex("0000000000000000000000000000000036e5f6b5c5e06070f0efca96227a863e"), bytes.fromhex("416e79207375626d697373696f6e20746f20746865204945544620696e74656e6465642062792074686520436f6e7472696275746f7220666f72207075626c69636174696f6e20617320616c6c206f722070617274206f6620616e204945544620496e7465726e65742d4472616674206f722052464320616e6420616e792073746174656d656e74206d6164652077697468696e2074686520636f6e74657874206f6620616e204945544620616374697669747920697320636f6e7369646572656420616e20224945544620436f6e747269627574696f6e222e20537563682073746174656d656e747320696e636c756465206f72616c2073746174656d656e747320696e20494554462073657373696f6e732c2061732077656c6c206173207772697474656e20616e6420656c656374726f6e696320636f6d6d756e69636174696f6e73206d61646520617420616e792074696d65206f7220706c6163652c207768696368206172652061646472657373656420746f")).hex())
out.append("mac A.3#3 " + poly1305_tag(bytes.fromhex("36e5f6b5c5e06070f0efca96227a863e00000000000000000000000000000000"), bytes.fromhex("416e79207375626d697373696f6e20746f20746865204945544620696e74656e6465642062792074686520436f6e7472696275746f7220666f72207075626c69636174696f6e20617320616c6c206f722070617274206f6620616e204945544620496e7465726e65742d4472616674206f722052464320616e6420616e792073746174656d656e74206d6164652077697468696e2074686520636f6e74657874206f6620616e204945544620616374697669747920697320636f6e7369646572656420616e20224945544620436f6e747269627574696f6e222e20537563682073746174656d656e747320696e636c756465206f72616c2073746174656d656e747320696e20494554462073657373696f6e732c2061732077656c6c206173207772697474656e20616e6420656c656374726f6e696320636f6d6d756e69636174696f6e73206d61646520617420616e792074696d65206f7220706c6163652c207768696368206172652061646472657373656420746f")).hex())
out.append("mac A.3#4 " + poly1305_tag(bytes.fromhex("1c9240a5eb55d38af333888604f6b5f0473917c1402b80099dca5cbc207075c0"), bytes.fromhex("2754776173206272696c6c69672c20616e642074686520736c6974687920746f7665730a446964206779726520616e642067696d626c6520696e2074686520776162653a0a416c6c206d696d737920776572652074686520626f726f676f7665732c0a416e6420746865206d6f6d65207261746873206f757467726162652e")).hex())
out.append("mac A.3#5 " + poly1305_tag(bytes.fromhex("0200000000000000000000000000000000000000000000000000000000000000"), bytes.fromhex("ffffffffffffffffffffffffffffffff")).hex())
out.append("mac A.3#6 " + poly1305_tag(bytes.fromhex("02000000000000000000000000000000ffffffffffffffffffffffffffffffff"), bytes.fromhex("02000000000000000000000000000000")).hex())
out.append("mac A.3#7 " + poly1305_tag(bytes.fromhex("0100000000000000000000000000000000000000000000000000000000000000"), bytes.fromhex("fffffffffffffffffffffffffffffffff0ffffffffffffffffffffffffffffff11000000000000000000000000000000")).hex())
out.append("mac A.3#8 " + poly1305_tag(bytes.fromhex("0100000000000000000000000000000000000000000000000000000000000000"), bytes.fromhex("fffffffffffffffffffffffffffffffffbfefefefefefefefefefefefefefefe01010101010101010101010101010101")).hex())
out.append("mac A.3#9 " + poly1305_tag(bytes.fromhex("0200000000000000000000000000000000000000000000000000000000000000"), bytes.fromhex("fdffffffffffffffffffffffffffffff")).hex())
out.append("mac A.3#10 " + poly1305_tag(bytes.fromhex("0100000000000000040000000000000000000000000000000000000000000000"), bytes.fromhex("e33594d7505e43b900000000000000003394d7505e4379cd01000000000000000000000000000000000000000000000001000000000000000000000000000000")).hex())
out.append("mac A.3#11 " + poly1305_tag(bytes.fromhex("0100000000000000040000000000000000000000000000000000000000000000"), bytes.fromhex("e33594d7505e43b900000000000000003394d7505e4379cd010000000000000000000000000000000000000000000000")).hex())

out.append("seal 2.8.2 " + aead_seal(bytes.fromhex("808182838485868788898a8b8c8d8e8f909192939495969798999a9b9c9d9e9f"), bytes.fromhex("070000004041424344454647"), bytes.fromhex("4c616469657320616e642047656e746c656d656e206f662074686520636c617373206f66202739393a204966204920636f756c64206f6666657220796f75206f6e6c79206f6e652074697020666f7220746865206675747572652c2073756e73637265656e20776f756c642062652069742e"), bytes.fromhex("50515253c0c1c2c3c4c5c6c7")).hex())
out.append(aead_open_line("open 2.8.2", bytes.fromhex("808182838485868788898a8b8c8d8e8f909192939495969798999a9b9c9d9e9f"), bytes.fromhex("070000004041424344454647"), bytes.fromhex("d31a8d34648e60db7b86afbc53ef7ec2a4aded51296e08fea9e2b5a736ee62d63dbea45e8ca9671282fafb69da92728b1a71de0a9e060b2905d6a5b67ecd3b3692ddbd7f2d778b8c9803aee328091b58fab324e4fad675945585808b4831d7bc3ff4def08e4b7a9de576d26586cec64b6116") + bytes.fromhex("1ae10b594f09e26a7e902ecbd0600691"), bytes.fromhex("50515253c0c1c2c3c4c5c6c7")))

out.append(aead_open_line("open A.5", bytes.fromhex("1c9240a5eb55d38af333888604f6b5f0473917c1402b80099dca5cbc207075c0"), bytes.fromhex("000000000102030405060708"), bytes.fromhex("64a0861575861af460f062c79be643bd5e805cfd345cf389f108670ac76c8cb24c6cfc18755d43eea09ee94e382d26b0bdb7b73c321b0100d4f03b7f355894cf332f830e710b97ce98c8a84abd0b948114ad176e008d33bd60f982b1ff37c8559797a06ef4f0ef61c186324e2b3506383606907b6a7c02b0f9f6157b53c867e4b9166c767b804d46a59b5216cde7a4e99040c5a40433225ee282a1b0a06c523eaf4534d7f83fa1155b0047718cbc546a0d072b04b3564eea1b422273f548271a0bb2316053fa76991955ebd63159434ecebb4e466dae5a1073a6727627097a1049e617d91d361094fa68f0ff77987130305beaba2eda04df997b714d6c6f2c29a6ad5cb4022b02709b") + bytes.fromhex("eead9d67890cbb22392336fea1851f38"), bytes.fromhex("f33388860000000000004e91")))


# --- the same tiny deterministic generator as t_chacha20poly1305.m31's `Rng` -

class Rng:
    def __init__(self, seed):
        self.state = seed

    def next(self):
        self.state = (self.state * 1_103_515_245 + 12_345) & 0x7FFF_FFFF
        return self.state

    def gen(self, n):
        return bytes(self.next() & 0xFF for _ in range(n))

    def range(self, n):
        return self.next() % n


# --- length sweep: every ChaCha20/Poly1305 length from 0 to 260 octets ------

sweep_rng = Rng(1)
sweep_key = sweep_rng.gen(32)
sweep_nonce = sweep_rng.gen(12)
pattern = bytearray()
sweep_len = 0
while sweep_len <= 260:
    out.append("sweep-encrypt " + str(sweep_len) + " " + chacha_encrypt(sweep_key, 0, sweep_nonce, bytes(pattern)).hex())
    out.append("sweep-mac " + str(sweep_len) + " " + poly1305_tag(sweep_key, bytes(pattern)).hex())
    pattern.append((sweep_len * 7 + 13) & 0xFF)
    sweep_len += 1

# --- length sweep: AEAD over the AAD/plaintext length grid ------------------

SWEEP_LENGTHS = [0, 1, 15, 16, 17, 31, 32, 33, 47, 48, 49, 63, 64, 65, 100, 127, 128, 129, 200]

grid_rng = Rng(2)
for aad_len in SWEEP_LENGTHS:
    for pt_len in SWEEP_LENGTHS:
        key = grid_rng.gen(32)
        nonce = grid_rng.gen(12)
        aad = grid_rng.gen(aad_len)
        pt = grid_rng.gen(pt_len)
        sealed = aead_seal(key, nonce, pt, aad)
        out.append(f"sweep-seal {aad_len}-{pt_len} " + sealed.hex())
        # The round trip and the single-bit-tamper check are verified inside
        # t_chacha20poly1305.m31 itself (both are properties of this
        # library alone, needing no external oracle); only the sealed bytes
        # are compared here. Still, draw the same "flip_at" value from the
        # RNG stream so every later call's inputs line up between the two
        # files.
        if len(sealed) > 0:
            grid_rng.range(len(sealed))

# --- 900 further cases against `cryptography`: 300 each of raw ChaCha20, ---
# raw Poly1305, and the full AEAD --------------------------------------------

FUZZ_N = 300

enc_rng = Rng(3)
for i in range(FUZZ_N):
    key = enc_rng.gen(32)
    nonce = enc_rng.gen(12)
    counter = enc_rng.range(4)
    pt = enc_rng.gen(enc_rng.range(600))
    out.append(f"fuzz-encrypt {i} " + chacha_encrypt(key, counter, nonce, pt).hex())

mac_rng = Rng(4)
for i in range(FUZZ_N):
    key = mac_rng.gen(32)
    msg = mac_rng.gen(mac_rng.range(600))
    out.append(f"fuzz-mac {i} " + poly1305_tag(key, msg).hex())

aead_rng = Rng(5)
for i in range(FUZZ_N):
    key = aead_rng.gen(32)
    nonce = aead_rng.gen(12)
    aad = aead_rng.gen(aead_rng.range(200))
    pt = aead_rng.gen(aead_rng.range(600))
    sealed = aead_seal(key, nonce, pt, aad)
    out.append(f"fuzz-seal {i} " + sealed.hex())
    if len(sealed) > 0:
        aead_rng.range(len(sealed))

print("\n".join(out))
