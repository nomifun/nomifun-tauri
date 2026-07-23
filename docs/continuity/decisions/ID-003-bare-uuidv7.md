# ID-003：无前缀、固定结构 UUIDv7

状态：`ACCEPTED`

提出时间：2026-07-21

前置决策：`ID-002-layered-id-v3.md`

## 决策

对于确实需要跨库、导入导出、文件引用、公开 URL 或分布式写入的具名
业务 ID，新值统一使用无业务前缀的标准 UUIDv7：

```text
0190f5fe-7c00-7a00-8000-000000000003
```

固定结构：

- 36 个字符；
- 小写十六进制；
- 标准 `8-4-4-4-12` 连字符结构；
- UUID version 7；
- RFC variant；
- 不添加 `conv_`、`msg_`、`prov_` 等前缀。

不使用 32 字符自定义紧凑格式，以避免再次增加私有协议和转换成本。

## 类型识别

类型由固定字段名和领域类型表达：

```text
conversations.conversation_id
messages.message_id
providers.provider_id
```

Rust 使用 `ConversationId(Uuid)` 等领域 newtype；TypeScript 使用按领域
区分的 brand。不得从 UUID 字符串内容推断实体类型。

## 不适用范围

以下内容不强制 UUIDv7：

- 每张 SQLite 产品持久表自身的自增整数 `id`；
- 自然键：name、tag、server_url、skill_name、model；
- 外部 ID：平台用户、ACP session、remote task、provider request；
- 操作、幂等和运行时 token；
- 依附父表的一对一状态和缓存。

## v3 代际边界

- v3 不读取或迁移历史 `prefix_UUIDv7`、旧短 ID 和数字业务 ID；
- 新写入不再产生带前缀 ID；
- 不建立历史 ID 映射；
- 旧数据集整体隔离后，新数据集重新生成 UUIDv7；
- v3 所有 S 类字段使用统一固定结构。

## Schema 示例

```sql
CREATE TABLE conversations (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    conversation_id TEXT NOT NULL UNIQUE
        CHECK (
            length(conversation_id) = 36
            AND lower(conversation_id) = conversation_id
            AND conversation_id
                GLOB '????????-????-7???-[89ab]???-????????????'
            AND replace(conversation_id, '-', '')
                NOT GLOB '*[^0-9a-f]*'
        )
);
```

SQLite 的 CHECK 负责基础形状，Rust 领域类型负责 version/variant 的完整
验证。
