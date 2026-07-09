# WeChat iLink outbound media-upload — discovery findings (Phase C1)

**Date:** 2026-07-09
**Status:** ✅ **IMPLEMENTED** (2026-07-09, same branch). The exact contract below was recovered from the reference SDK (`openclaw-weixin`) and ported to `weixin/{types.rs,api.rs,plugin.rs}`. `send_media` uploads AES-128-ECB-encrypted bytes to the WeChat CDN and sends an image/file item. AES-ECB+PKCS7 has offline roundtrip unit tests; the live CDN round-trip is **still unverified** (needs a logged-in bot). See "Implementation notes" at the bottom.

## Original gate note (superseded)

The Phase C1 spike deferred implementation pending the exact field schema. Those schemas were then recovered from the reference SDK source (below), so the track was un-gated and completed.

## The contract (flow, confirmed)

Gateway `https://ilinkai.weixin.qq.com`, CDN `https://novac2c.cdn.weixin.qq.com`. This is Tencent's **official** personal-account Bot API (OpenClaw ClawBot / iLink protocol) — legal, server-side, supports images/voice/files/video natively. Our code already integrates the text path and already **parses inbound** encrypted media (`weixin/types.rs:96-141` `MediaItemData { media: { encrypt_query_param, aes_key }, aeskey, file_name }`) — outbound is symmetric.

Outbound image flow:
1. Generate a random **AES-128** key.
2. **AES-128-ECB** encrypt the image bytes (all iLink CDN media is AES-128-ECB encrypted).
3. `POST /ilink/bot/getuploadurl` with `{ filekey, md5, len }` → a pre-signed CDN upload URL (+ reference params).
4. **PUT** the encrypted bytes to that CDN URL.
5. `POST /ilink/bot/sendmessage` with the standard `msg` wrapper (`message_type: 2`, `message_state: 2`, **`context_token` from the inbound message — required**) and an `item_list` entry of **`type: 2` (ITEM_TYPE_IMAGE)** carrying the base64 `aes_key` + the CDN reference params (mirrors the inbound `MediaItemData` shape).

Headers already implemented for text send apply (`AuthorizationType: ilink_bot_token`, `Authorization: Bearer <bot_token>`, `X-WECHAT-UIN`). `context_token` is already captured per-chat in `weixin/plugin.rs` (`self.context_tokens`).

## What implementation would take (ready-to-do once schema confirmed)

- **New dependency:** ECB mode. `aes` 0.8 is in the tree but the `ecb` crate is not; add `ecb` (or drive `aes` block-by-block with PKCS7) to the `weixin` feature. `base64` + `uuid` are already `weixin` deps.
- **`weixin/types.rs`:** add outbound `SendImageItem` (fields: `aeskey`, CDN reference — copy names from the inbound `MediaItemData`/`MediaEncryptInfo` and a reference SDK) and extend `SendMessageItem` with optional `image_item`/`file_item` (currently text-only, `types.rs:164-170`). `ITEM_TYPE_IMAGE=2`/`FILE=4` constants already exist (`types.rs:8-12`).
- **`weixin/api.rs`:** add `get_upload_url(filekey, md5, len)`, a CDN `PUT`, and `send_image(to_user_id, image_ref, context_token)`; add the AES-128-ECB encrypt helper.
- **`weixin/plugin.rs`:** override `send_media` — encrypt `media.bytes` → getuploadurl → PUT → send_image (reuse `self.context_tokens.get(chat_id)`).
- **Verify:** live WeChat bot round-trip (no automated test possible).

## Reference sources (reverse-engineered community SDKs with exact schemas)

- iLink Bot API protocol — https://www.wechatbot.dev/zh/protocol
- openclaw-weixin `weixin-bot-api.md` (full curl + payloads) — https://github.com/hao-ji-xing/openclaw-weixin/blob/main/weixin-bot-api.md
- XTmai/WeChat-iLinkBot (Python: QR login + send text/file/image) — https://github.com/XTmai/WeChat-iLinkBot
- x1ah/wechat-ilink-demo (Node `demo.mjs`, independent iLink calls) — https://github.com/x1ah/wechat-ilink-demo

The `demo.mjs` / `weixin-bot-api.md` in those repos carry the exact image `item_list` JSON and `getuploadurl` request/response — read one of them to fill `SendImageItem` field names before implementing.

## Implementation notes (as built)

Recovered the exact schemas from `hao-ji-xing/openclaw-weixin` (`src/cdn/*.ts`, `src/messaging/send.ts`, `src/api/types.ts`) and ported them:

- **`getuploadurl`** (`POST ilink/bot/getuploadurl`, no `base_info`): `{ filekey (16B hex), media_type (IMAGE=1 / FILE=3 — the proto `UploadMediaType`, NOT the item type), to_user_id, rawsize, rawfilemd5 (plaintext md5 hex), filesize (ciphertext padded size), no_need_thumb: true, aeskey (16B hex) }` → `{ upload_param }`.
- **CDN upload**: `POST https://novac2c.cdn.weixin.qq.com/c2c/upload?encrypted_query_param=<upload_param>&filekey=<filekey>`, `Content-Type: application/octet-stream`, body = AES-128-ECB(PKCS7) ciphertext, **no gateway auth**. The download reference comes back in the **`x-encrypted-param` response header** → goes into `media.encrypt_query_param`.
- **`sendmessage` item** (`message_type:2`, `message_state:2`, `context_token` required): image → `{ type:2, image_item:{ media:{ encrypt_query_param, aes_key, encrypt_type:1 }, mid_size:<ciphertextSize> } }`; file → `{ type:4, file_item:{ media:{…}, file_name, len:<plaintextSize as string> } }`.
- **AES**: AES-128-ECB, PKCS7, random 16-byte key; ciphertext size = `ceil((n+1)/16)*16` (a block-aligned plaintext still gets a full padding block).
- **Quirk replicated exactly**: `media.aes_key` = base64 of the AES key's **hex-string bytes** (32 ASCII chars), not the raw 16 key bytes — this is what the reference SDK sends, so we match it byte-for-byte.

Code: `weixin/types.rs` (`GetUploadUrlRequest`/`Response`, `SendCdnMedia`, `SendImageItem`, `SendFileItem`, extended `SendMessageItem`, `UPLOAD_MEDIA_TYPE_*`); `weixin/api.rs` (`aes128_ecb_pkcs7_encrypt`, `aes_ecb_padded_size`, `get_upload_url` via `authenticated_post`, `upload_to_cdn`, `send_media`); `weixin/plugin.rs` (`send_media` override using the per-chat `context_token`). New deps: `aes`, `md-5` (both already in the lock), `hex` (added to the `weixin` feature).

**Still unverified:** the live CDN round-trip and server acceptance — the AES/PKCS7 has offline roundtrip tests, but no logged-in WeChat bot was available to exercise the full path. Verify on a real bot before relying on it.
