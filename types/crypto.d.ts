// Type declarations for Aike JS runtime — Web Crypto API
// (WHATWG Web Crypto API subset, see memo/api-todo.md P0).

// ---------------------------------------------------------------------------
// BufferSource
// ---------------------------------------------------------------------------

/** Binary data source: an ArrayBuffer or any ArrayBufferView (TypedArray, DataView). */
type BufferSource = ArrayBuffer | ArrayBufferView;

/** Base for algorithm parameter dictionaries: identifies the algorithm by name.
 *  (Web Crypto `Algorithm`; the `*Params` interfaces below extend it.) */
interface Algorithm {
  name: string;
}

/** An algorithm identifier: a case-insensitive algorithm name string, or a
 *  dictionary with a `name` member. */
type AlgorithmIdentifier = string | { name: string; [key: string]: any };

/** The type of a CryptoKey. */
type KeyType = "secret" | "public" | "private";

/** Usages a CryptoKey can be created with. */
type KeyUsage =
  | "encrypt"
  | "decrypt"
  | "sign"
  | "verify"
  | "deriveKey"
  | "deriveBits"
  | "wrapKey"
  | "unwrapKey";

/** Import/export formats supported by this implementation. */
type KeyFormat = "raw" | "jwk";

/** A JSON Web Key (RFC 7517). Only `oct` symmetric keys are supported. */
interface JsonWebKey {
  /** Key type; always "oct" for symmetric keys. */
  kty?: string;
  /** Base64url-encoded key material. */
  k?: string;
  /** Algorithm (e.g. "HS256", "A128GCM"). */
  alg?: string;
  /** Whether the key is extractable. */
  ext?: boolean;
  /** Intended key usages. */
  key_ops?: string[];
  /** Intended use ("sig" for HMAC, "enc" for AES). */
  use?: string;
}

// ---------------------------------------------------------------------------
// CryptoKey
// ---------------------------------------------------------------------------

/**
 * A cryptographic key. Cannot be constructed directly; obtained from
 * `crypto.subtle.importKey()`.
 */
declare class CryptoKey {
  private constructor();
  /** Key type. */
  readonly type: KeyType;
  /** Whether the key material can be exported. */
  readonly extractable: boolean;
  /** Key algorithm, e.g. `{ name: "HMAC", hash: "SHA-256", length: 256 }`. */
  readonly algorithm: object;
  /** Usages granted at import time. */
  readonly usages: KeyUsage[];
}

// ---------------------------------------------------------------------------
// SubtleCrypto
// ---------------------------------------------------------------------------

/** Parameters for AES-GCM encryption/decryption. */
interface AesGcmParams extends Algorithm {
  /** The initialization vector (required). */
  iv: BufferSource;
  /** Additional authenticated data. */
  additionalData?: BufferSource;
  /** Tag length in bits. One of 32, 64, 96, 104, 112, 120, 128. Default 128. */
  tagLength?: number;
}

/** Parameters for HMAC sign/verify. */
interface HmacKeyGenParams extends Algorithm {
  /** The hash algorithm, e.g. "SHA-256". */
  hash: AlgorithmIdentifier;
}

/** Parameters for importing an HMAC key. */
interface HmacImportParams extends Algorithm {
  /** The hash algorithm, e.g. "SHA-256" (required for import). */
  hash: AlgorithmIdentifier;
}

/**
 * The Web Crypto `SubtleCrypto` interface (accessed as `crypto.subtle`).
 * Cannot be constructed directly. All methods return Promises; errors are
 * surfaced as rejections.
 */
declare class SubtleCrypto {
  private constructor();
  /** Hash the given data with SHA-1/SHA-256/SHA-384/SHA-512. */
  digest(
    algorithm: AlgorithmIdentifier,
    data: BufferSource,
  ): Promise<ArrayBuffer>;
  /** Generate an HMAC signature over `data`. */
  sign(
    algorithm: AlgorithmIdentifier,
    key: CryptoKey,
    data: BufferSource,
  ): Promise<ArrayBuffer>;
  /** Verify an HMAC signature over `data`. */
  verify(
    algorithm: AlgorithmIdentifier,
    key: CryptoKey,
    signature: BufferSource,
    data: BufferSource,
  ): Promise<boolean>;
  /** AES-GCM encrypt. Returns ciphertext || authentication tag. */
  encrypt(
    algorithm: AesGcmParams,
    key: CryptoKey,
    data: BufferSource,
  ): Promise<ArrayBuffer>;
  /** AES-GCM decrypt. Throws `OperationError` if the tag is invalid. */
  decrypt(
    algorithm: AesGcmParams,
    key: CryptoKey,
    data: BufferSource,
  ): Promise<ArrayBuffer>;
  /** Import a symmetric key ("raw" bytes or a JSON Web Key). */
  importKey(
    format: KeyFormat,
    keyData: BufferSource | JsonWebKey,
    algorithm: AlgorithmIdentifier | HmacImportParams,
    extractable: boolean,
    keyUsages: KeyUsage[],
  ): Promise<CryptoKey>;
  /** Export a symmetric key as raw bytes or a JSON Web Key. */
  exportKey(
    format: KeyFormat,
    key: CryptoKey,
  ): Promise<ArrayBuffer | JsonWebKey>;
}

// ---------------------------------------------------------------------------
// Crypto
// ---------------------------------------------------------------------------

/**
 * The Web Crypto `Crypto` interface. The global `crypto` object is the only
 * instance; it cannot be constructed.
 */
declare class Crypto {
  private constructor();
  /** The SubtleCrypto interface for low-level operations. */
  readonly subtle: SubtleCrypto;
  /**
   * Fill an integer-typed array with cryptographically secure random bytes
   * and return the same array. Throws `TypeError` for float arrays / DataView
   * and `QuotaExceededError` above 65536 bytes.
   */
  getRandomValues<T extends ArrayBufferView>(array: T): T;
  /** Generate a random version-4 UUID string. */
  randomUUID(): string;
  /**
   * Aike extension: SHA-256 of `data` as a lowercase hex string.
   * A string is encoded as UTF-8; BufferSource is hashed directly.
   */
  sha256Hex(data: string | BufferSource): string;
}

/** The global Web Crypto object. */
declare const crypto: Crypto;
