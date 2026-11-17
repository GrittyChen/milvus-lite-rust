# 0036：jieba 分词

状态：设计已通过（2026-11-17，经一轮独立评审并按评审修改），待实施第 1 步。动手前的调查已完成（见「调查结论」）。负责人确认的方向：
- 以 jieba-rs 为依赖，不追求在全部输入上与 Python 版 jieba 逐位一致，差异显式登记；
- 3.x 建的 jieba 集合照常打开；
- 用户词按每个集合各自的词典处理，列表中的每一项按 Python `add_word` 的语义各执行一次。

参照版本 `1320d26`（CPython 3.12），对照的 jieba 为 0.42.1。

## 范围

本切片支持 BM25 输入字段的 `analyzer_params` 中的 jieba 分词器，覆盖：建表；打开集合，包括 3.x 写出的目录；写入时生成 BM25 输出字段；以文本为查询的 BM25 检索。[0031](0031-bm25.md) 把 jieba 列在范围外，建表与打开一直返回 UNIMPLEMENTED。

不包含：

- `text_match`：3.x 的 `text_match` 不读 `analyzer_params`，固定用 standard 分词（[0029](0029-text-match.md)），与 jieba 无关。
- jieba 的词性标注、关键词抽取（TF-IDF / TextRank）、`cut_all`，以及直接对外的 `HMM=False` 切分：3.x 都不用。`HMM=False` 的 `cut` 只在 `suggest_freq` 内部用到。
- 运行期加载外部词典文件：3.x 只接受 `analyzer_params` 里的词列表。

## 3.x 的行为

**依赖**：jieba 是可选依赖（`milvus-lite[chinese]`，`jieba>=0.42`）。满足条件的只有 0.42 与 0.42.1，实际安装的是 0.42.1（2020 年发布，此后没有新版本）。两个版本都只有 sdist、没有 wheel。逐文件比较两者，除版本号与打包元数据外，只有 paddle 分支里一处判空写法不同；词典、finalseg、`cut` 与 `add_word` 完全相同。下文以 0.42.1 为准。没装 jieba 时，构造分词器报 code 1 `JiebaAnalyzer requires the 'jieba' package. Install it with: pip install jieba`（0031 已实测）。

**选择分词器**（[factory.py](https://github.com/milvus-io/milvus-lite/blob/1320d26/milvus_lite/analyzer/factory.py)）：standard 部分的判定与文案见 0031，本节只列与 jieba 相关的规则。

- `tokenizer` 为字符串 `"jieba"`：`mode` 为 search，没有用户词。
- `tokenizer` 为对象且 `type == "jieba"`：读对象里的 `mode` 与 `dict`。没有 `tokenizer` 键时改读顶层的 `type`，按同样的规则处理：`"jieba"` 字符串等同于字符串形式；对象同样读其中的 `mode` 与 `dict`。`tokenizer` 为空对象时，同样改读顶层的 `type`。
- `mode`：先取 `mode or "search"`，之后只有等于字符串 `"search"` 才用 `cut_for_search`，其余一律用 `cut`。
  - search：`None`、`""`、`0`、`false`；
  - exact：`"exact"`、`"SEARCH"`、`1`、`["x"]`。
- `dict`：原值交给 `JiebaAnalyzer`，为真值时逐项调用 `jieba.add_word(word)`。以下是 Python 层直接调用 `create_analyzer` 的实测结果，经 gRPC 后的错误码与文案留在第 1 步核实：
  - 字符串：逐字符加入。
  - 对象：按键迭代。
  - `null`、`[]`、`{}`、`0`、`0.0`：等同于没有用户词。
  - 整数（非零）或 `true`：`TypeError: '<类型>' object is not iterable`。
  - 列表中有非字符串元素（`5`、`null`、`1.5`、`["a"]`）：`AttributeError: '<类型>' object has no attribute 'decode'`，来自 `strdecode`。
  - `[""]` 成功（空串的影响见下）。
  - Milvus 用 `"_default_"` 表示默认词典，3.x 里它没有特殊含义，按普通词加入。
- `hmm` 键不读，始终开 HMM。
- 停用词：取自 `filter` 中 `type == "stop"` 的对象，每一项都经 `str()` 转换（`factory.py` 的 `_extract_stop_words`）。standard 的词项只含 `\w`，所以本实现现有的 `stop_words()` 丢掉了浮点数与布尔值这类不可能命中的项。jieba 的词项可以是 `1.5`、`0.5%` 或标点，浮点停用词 `1.5` 会变成 `'1.5'` 并命中。`True` / `None` 经 `str()` 得到首字母大写的形式，词项已转为小写，所以永远不会命中。

**切分后的处理**（[jieba_analyzer.py](https://github.com/milvus-io/milvus-lite/blob/1320d26/milvus_lite/analyzer/jieba_analyzer.py)）：
- 先切词，再逐个词项处理：`strip()`、丢掉空串、`lower()`、去掉停用词。
- `lower()` 作用在单个词项上，没有整段文本的上下文：词尾 sigma 规则只看词项本身。例如单独成词的 `Σ` 得到 `σ`，不是 `ς`。
- 标点（`，`、`。`）不会被去掉，作为独立的词项参与 BM25 计数与哈希。
- 词项哈希与 standard 相同（FNV-1a，见 0031）。

**构造时机**：每个 BM25 function 构造两次分词器，一次在写入链（[function/builder.py:64](https://github.com/milvus-io/milvus-lite/blob/1320d26/milvus_lite/function/builder.py#L64)），一次给检索做查询分词（[engine/collection.py:317](https://github.com/milvus-io/milvus-lite/blob/1320d26/milvus_lite/engine/collection.py#L317)）。建表与打开都会构造。

**jieba 0.42.1 的切分**：
- 主切分：用前缀词典构造 DAG，按 `log(freq or 1) - log(total) + route[x+1]` 求概率最大的路径，同分取更大的 `x`。
- HMM：连续单字组成的缓冲不在词典里时，交给 finalseg 的 HMM（Viterbi，`MIN_FLOAT = -3.14e100`）。
- 字符划分：
  - 汉字块按 `re_han_default = [一-鿕a-zA-Z0-9+#&._%-]+` 划分；
  - 块外按 `(\r\n|\s)` 切，其余字符逐个输出；
  - HMM 内部按 `[一-鿕]+` 与 `[a-zA-Z0-9]+(?:\.\d+)?%?` 再切。
- `cut_for_search`：在 `cut` 的每个词上，对长度大于 2 的词输出词典里有的二字子串，长度大于 3 的再输出三字子串，最后输出词本身。
- `total`：加载词典时每一行都累加进 `total`。`dict.txt` 共 349046 行，其中「B超」重复一行，`total` 为 60101967。

**用户词与全局状态**：`add_word(word)` 修改的是进程全局的默认分词器 `jieba.dt`。
- 频率取 `suggest_freq(word)`：对 `cut(word, HMM=False)` 的每一段依次乘 `FREQ.get(seg, 1) / total`，再取 `max(int(freq * total) + 1, FREQ.get(word, 1))`。只是某个词的前缀、本身不是词的条目，频率为 0。
- 写入时 `FREQ[word] = freq`，`total += freq`，并补齐该词的前缀条目（频率 0）。词已存在时，旧频率不从 `total` 中减去。
- 空串：`suggest_freq("")` 为 `total + 1`，加入后 `total` 约翻倍，之后的切分全部受影响。
- 列表里的重复词会再加一次，`total` 再增加。
- 由于状态是全局的，一个集合的用户词会影响同一进程里**所有** jieba 集合，包括没有用户词的集合；每次构造又会再加一遍，`total` 随打开次数增长。带用户词的集合，结果取决于进程里集合的打开顺序和次数，不可复现。

## 调查结论

调查在仓库外进行，材料由负责人本地留存：jieba 0.42 与 0.42.1 的 sdist（PyPI；0.42.1 的 sha256 为 `055ca12f…928db2`）、jieba-rs 的源码（`bac49e6`）、crates.io 上 0.11.0 的发行包、探针与对照脚本。

**jieba-rs 0.11.0 与 `bac49e6`**：
- crates.io 上 0.11.0 的发行包记录的源码提交是 `v0.11.0` tag（`3a0d75c`），其 `src` 与该 tag 下的 `jieba/` 逐文件相同。
- `v0.11.0` 到 `bac49e6` 之间，`jieba/` 与 `jieba-macros/` 只有两处改动：可选的 `ts-rs` 特性，以及 `cfg(target_family = "wasm")` 下的改动。非 wasm 目标的行为没有变化。

**jieba-rs 默认配置与 jieba 0.42.1 的四处差异**，都能在本实现的包装层修正，不需要改 jieba-rs：

| # | jieba-rs 默认 | 修正 |
| --- | --- | --- |
| 1 | 主切分的汉字范围到 `鿿`，并包含扩展区 | 先按 Python 的 `re_han_default` 切块，只把汉字块交给 jieba-rs；块外按 Python 的规则自行处理 |
| 2 | 词典去重，`total` 为 60101964 | 加载 Python 版 `dict.txt` 后，补一个匹配不到的占位词（频率 3），使 `total` 与 Python 相同 |
| 3 | 内置 HMM 的发射概率舍入到 6 位小数 | 用 0.42.1 的 `prob_*.py` 生成全精度模型，经 `HmmModel::load` 替换 |
| 4 | HMM 把 ASCII 的 `.`、`-`、`_` 连成一个词，`cut_for_search` 还会拆出子词 | 不在词典里且含这三个字符的 ASCII 词，按 finalseg 的 `re_skip` 再切；`cut_for_search` 在修正后的 `cut` 上按 Python 的逻辑自行实现 |

**验证范围**：下文提到“验证范围”时，均指本节列出的输入与操作，决策 B、D 与差异表都以此为准。
- 语料：
  - 《围城》全文 317 行；
  - jieba 自带的测试句；
  - 200000 条随机混合串（种子 29），由常用词、`鿖` 以上与扩展区的汉字、ASCII 与 `+#&._%-`、Unicode 空白与中文标点、带小数与百分号的数字组成；
  - 11 条极端输入：整本《围城》作为一个字符串、`"a."*3000`、`"鿕"*3000`、两万个 `a`、一万个 `。`、`1.2.3.4.5.` 重复 500 次，以及含全角、组合字符、`\x00`、`\u0085`、零宽空格、BOM 与 emoji 的混合串。
- 操作：
  - `cut` 与 `cut_for_search`（`HMM=True`）；
  - `suggest_freq` 依赖的 `cut(HMM=False)`，在前 20000 条输入上对照；
  - 用户词：150 组随机用例，词取自词典词、拼接词、随机汉字、`Deep Learning`、`a.b`、`T.S.Eliot`、`C++`、空串、空格、换行与重复词，每组 8 个文本。比较 `total`、每个词的频率、`cut` 与 `cut_for_search`，与同一个新建的 `jieba.Tokenizer()` 对照。
- 平台：macOS arm64。切分比较的是 `log` 之和，两边都调用平台的 libm，所以只对同一平台上的结果作保证。0035 出过 libm 相差 1 ULP 的先例，差分与对照都在同一平台上运行，验收记录写明平台。`suggest_freq` 只用乘除，不受 libm 影响。

**对照结果**：
- 修正后，在验证范围内全部逐词相同，用户词的 `total` 与频率也逐一相同。
- jieba-rs 默认配置下，200317 条输入中 `cut` 有 21.2%、`cut_for_search` 有 22.5% 不同；其中真实文本有 13 行不同，共 55 处，全部属于第 4 类。
- 这只说明在这批输入上一致，不是对全部输入的证明。

**依赖**：
- `default-features = false` 时，普通依赖只有 bytecount、rustc-hash 与 jieba-macros（proc-macro）。三者都是纯 Rust，没有构建脚本，也没有 C 代码；许可为 MIT / Apache-2.0。
- 默认的 `default-dict` 经 include-flate 引入 zstd-sys（C）与 pkg-config，不开启。
- jieba-macros 在编译期内嵌舍入后的模型。本实现不用这份模型，但无法去掉它，约占几百 KB。

**资源**：
- 体积：Python 版词典（5071852 字节）与全精度模型（828901 字节）原始约 5.9 MB，deflate 后约 2.2 MB，即 wheel 的增量。
- 单个实例：加载约 50 ms，常驻约 42–46 MB（macOS arm64，release）。
- 每个带用户词的集合各持有一个完整实例。
- jieba-rs 每个线程有 thread-local 的临时缓冲，每个缓冲上限约 4 MiB，RPC 阻塞池默认最多 512 个线程。

**用户词规模**：jieba-rs 的 trie 在节点数达到 `1 << 24`（约 1677 万）时直接 `assert!` panic（`trie.rs` 的 `MAX_NODES`）。默认词典约占 49.8 万个节点，gRPC 单条消息最大 256 MiB，足够容纳一个超大的 `dict`，在建表时触发 panic。

**3.x 的两次构造**：3.x 对每个 BM25 function 构造两次分词器，所以在全新进程里只打开一次的集合，用户词实际加了两次。第二次只改变 `total`，不改变词的频率。8 组随机用户词（每组 2–4 个新造词）在各 3000 行文本上比较“加一次”与“加两次”，`cut` 与 `cut_for_search` 都没有差异。对已在词典里的高频词，`total` 的增量大，差异需要在第 1 步单独测量。

## 设计

- **数据**：
  - jieba 0.42.1 的 `dict.txt` 原样入库。sdist 不含 `LICENSE` 文件，只在 `setup.py` 中声明 MIT，许可文本与版权行取自上游仓库，入库时核对，并注明来源。
  - 全精度 HMM 模型由生成脚本从 `prob_*.py` 产出并入库，生成脚本带 `--verify`。
  - 两者都在编译期经 `include_bytes!` 嵌入，不读运行平台上的文件。
- **module `jieba`**（`crates/core`）：
  - 包装层与全部修正：预切块、占位词、全精度模型、ASCII 再切、`cut_for_search`；
  - 用户词的 `suggest_freq` 与 `add_word`；
  - 切分后逐词项做 `strip`、`lower` 与去停用词。`strip` 用 CPython 3.12 的 `str.isspace` 表（新增，与 `filter::unicode` 同一来源）。`lower` 复用 0029 的表，但按单个词项调用，不带整段文本的上下文。
- **实例**：
  - 没有用户词的集合共用一个进程级只读实例，首次用到时加载（`OnceLock`）。
  - 数据是编译期常量，加载失败只可能是构建缺陷，与输入无关。因此用单元测试证明它能加载，按规则 6 视为不会失败，不设计运行期的错误路径。
  - 带用户词的集合各自持有一个实例：从默认词典构造后，按列表顺序对**每一项**执行一次 Python 语义的 `add_word`，重复项也再执行一次。
  - 实例不可变，写入与检索都只读，不加锁。切分在调用方线程执行，与 standard 相同。
- **用户词的频率与存放**：
  - `suggest_freq` 按 Python 的运算顺序计算：逐段浮点乘、`int` 截断。
  - 前缀条目频率为 0 的规则，由按字典序排序的词表做前缀查询实现，这张表只在带用户词时构造。
  - jieba-rs 的 `add_word` 对已有词会先减去旧频率；`total` 的差额用占位词补齐，使每一步之后的 `total` 都与 Python 相同。
  - 含 `re_han_default` 字符集之外字符的用户词（空格、标点、空串等），在 `cut` 里永远匹配不到，也不参与二字、三字子串。它们只通过 `total`、前缀条目和“同词再加”影响结果，因此只记在包装层的覆盖表里，不进 jieba-rs 的 trie，也就不会与占位词冲突。
- **用户词上限**：建表、打开时先预算新增的 trie 节点数，超过上限即报错，不让 jieba-rs 的 `assert!` 到达 RPC。具体上限、错误码与文案在第 1 步确定，做法参照 0032 的资源上限。3.x 没有这个上限，登记为差异。
- **停用词**：jieba 路径按 Python 的 `str()` 收集每一项。
  - 浮点数用 Python `repr` 的格式，例如 `1.0`、`1e+20`；
  - 布尔值与 `null` 分别为 `True` / `False` / `None`；
  - 嵌套值同样按 Python 的 `str()`。

  实施时核对 `py_json_repr` 是否已覆盖这些格式。standard 路径的现有行为不变。
- **`Bm25Analyzer`**：分词器从「standard + 停用词」扩为 standard 或 jieba 两种。`from_params` 按上文 3.x 的规则解析 `mode` 与 `dict`；词项哈希、计数与 BM25 打分路径不变。
- **持久化**：
  - `analyzer_params` 已按原值保存与回显（0029）。本切片不新增任何文件或字段，数据目录格式不变。
  - 回退到本切片之前的版本（0.6.0rc1 及更早）时，带 jieba 的集合打开即返回 UNIMPLEMENTED。这是显式拒绝，数据保留，符合规则 8；README 与发行说明照 0.5.0rc1 对 BM25 降级的写法说明。
  - 稀疏向量在写入时按当时的切分生成，之后不重写。以后修正包装层（决策 B）时，同一集合里新旧切分的行会并存，发行说明须写明受影响的输入。

## 决策

- **A 依赖**：
  - 使用 jieba-rs 0.11.0（crates.io 2026-09-16 发布），`default-features = false`，版本写死。
  - 0.11.0 与调查所用的 `bac49e6` 在非 wasm 目标上没有行为差异（见「调查结论」），引入后在 0.11.0 上重跑一遍对照即可。
  - 引入时用 `cargo tree --workspace -e all` 核对完整依赖图。
  - 升级须重跑本切片的对照工具。
- **B 对齐口径**：
  - 没有用户词时，以 jieba 0.42.1（CPython 3.12）为真值，在验证范围内要求逐词相同。
  - 超出验证范围的输入若出现差异，按 bug 处理，修正包装层；确实无法修正时再登记。
- **C 用户词**：
  - 每个集合使用独立的词典，列表中的每一项按 Python `add_word` 的语义各执行一次，不复制全局共享与重复构造的累加。
  - 登记为差异：3.x 的结果取决于进程历史；本实现与“全新进程里只加一次”的状态相同。
  - 3.x 实际在每次构造时都加一遍，所以即使只打开一次，也是加了两次。对新造词，实测没有差异；对已在词典里的高频词，差异由第 1 步测量。
- **D 3.x 的目录**：3.x 建的 jieba 集合照常打开。
  - 没有用户词，且写入它的 3.x 进程从未加载过带用户词的 jieba 集合时，已存的稀疏向量与本实现的查询分词在验证范围内一致。
  - 有用户词，或 3.x 进程曾受其他集合的用户词污染时，两边的切法可能不同：不同的词项在检索时匹配不上，召回下降；之后本实现写入的行按本实现的切法生成，同一集合里两种切法并存。
- **E 0002 约束 8**：分词与 3.x 逐位一致的要求，对 jieba 放宽为 B 与 C 的口径；在 0002 中注明，并链接本文。
- **F 未装 jieba 的 3.x**：
  - 本实现无需另装任何东西即可使用 jieba。
  - 3.x 没装 jieba 时，建表与打开都报 code 1。
  - 本实现建的 jieba 集合交给 3.x 时，3.x 必须装 jieba 0.42 或 0.42.1，两者的算法与词典相同。
- **G 参照环境中的 jieba**：
  - 现状：CI 的固定依赖一律以 `uv pip install --no-build` 安装、只用 wheel（[.github/ci/README.md](../../.github/ci/README.md)），而 jieba 只有 sdist。所以 Compare 的参照环境、`tests/storage_interop` 的 Arrow 15 / 25 参照环境都装不上它。
  - 做法：参照 orjson 的先例，按 sha256 固定 jieba 0.42.1 的 sdist，在独立 job 中构建一次。在 [0021](0021-release-build-in-ci.md) 第 11 条与 0020 中登记这一例外，其余依赖仍只用 wheel。
  - 未采用的备选：把按哈希校验过的 sdist 解压后放进 `PYTHONPATH`。jieba 是纯 Python，这样不需要构建，但会在固定依赖之外另开一条安装路径。
  - 采用 orjson 的方式（负责人 2026-11-17 确认）。

## 与 3.x 的差异

| 情形 | 3.x | 本实现 | 理由 | 影响的用例 |
| --- | --- | --- | --- | --- |
| 没装 jieba | 建表、打开报 code 1 | 内置，照常可用 | 分词在服务端，内置是本实现的常态（决策 F） | BM25 + jieba 的差分在参照侧必须装 jieba；`bm25_cases.py` 原有的 jieba 登记项重写 |
| `dict` 用户词 | 进程全局，每次构造都累加，结果取决于打开顺序与次数 | 每个集合独立，每一项加一次 | 3.x 的行为不可复现（决策 C） | 带用户词的差分与互通，两边分别断言 |
| 无用户词的集合受同进程其他集合的用户词影响 | 受影响 | 不受影响 | 同上 | 同上；多集合用例在参照侧用新进程 |
| 用户词规模 | 无上限 | 超过 trie 节点上限时报错 | jieba-rs 在节点数达到 `1 << 24` 时 panic | 用户词上限的边界用例 |
| 验证范围之外的切分 | Python jieba 0.42.1 | jieba-rs 加包装层 | 只有对照过的输入保证相同（决策 B） | 无固定用例；由模糊对照覆盖 |
| 3.x 建的、带用户词或受污染的 jieba 集合 | 按 3.x 当时的进程状态写入 | 查询按本实现切分，部分词项匹配不上 | 决策 D | 3.x 写、本实现读的互通用例 |

0031 差异表里的「jieba 分词器」一行改为指向本节。

## 实施前核实（第 1 步先做，结果写回本文）

在仓库外另建一个 venv，给 3.x 参照环境装上 jieba 0.42.1，不改动现有的参照环境。实测以下内容：

1. `mode` / `dict` 的各种形状经 gRPC 后的错误码、文案与时机（建表时还是打开时）。Python 层的异常类型已在「3.x 的行为」中列出。
2. 已在词典里的高频词作为用户词时，“加两次”（3.x 新进程打开一次）与“加一次”（本实现）的切分差异大小。
3. 端到端：3.x 建表写入、本实现打开检索，以及反方向，比较结果与分数是否一致。
4. 用户词上限的取值，以及超限时的错误码与文案。
5. 决策 G 的安装路径能在 CI 的 Linux / macOS 参照环境中走通。

## 实施步骤

1. **真值与组件**：
   - 完成上节的实测；
   - 实现 `jieba` module、数据与生成脚本，接入 `Bm25Analyzer`，写单元测试；
   - 对照工具：`tests/differential/fuzz_jieba.py` 加 example 探针，形式同 standard 分词的模糊对照，种子固定；
   - 固定真值 fixture：`generate_jieba_fixtures.py --verify`。语料用 jieba 自带的测试句与生成的随机串，不把受版权保护的文本入库。
2. **差分、互通、参照用例与文档**：
   - BM25 + jieba 的差分，参照环境按决策 G 装 jieba 0.42.1，2.6 线同步；
   - 与 3.x 的双向互通用例；
   - 参照 `tests/` 里的 jieba 用例（`tests/analyzer/test_analyzer.py`、`tests/adapter/test_issue_19_20.py`）直接导入 3.x 的内部模块，在 [INVENTORY.md](../../tests/python/adapter/INVENTORY.md) 中属于“依赖内部模块”一类，不能直接复用。只把其中的断言改写为 gRPC 层用例，例如“深度学习”出现在词项中、自定义词与停用词“的”。
   - 同步现有代码、测试与文档中的 jieba 表述：
     - 代码：[analyzer.rs](../../crates/core/src/analyzer.rs) 的 UNIMPLEMENTED 分支及其单测。
     - 测试：[bm25.rs](../../crates/core/tests/bm25.rs) 中对 UNIMPLEMENTED 的断言；[bm25_cases.py](../../tests/differential/bm25_cases.py) 的用例与 `REGISTERED` 中的 jieba 项（参照侧装了 jieba 后不再是 code 1）。
     - 决策：
       - [0006](0006-m2-scope.md)：“使用 jieba 分词器的 BM25 输入仍拒绝打开”一句，以及差异表的第 ③ 项；
       - [0031](0031-bm25.md)：范围外一节、报 code 1 的文案、差异表；
       - [0029](0029-text-match.md)：不包含 Jieba 的表述；
       - [0002](0002-data-dir-compat.md)：“jieba 本身的等价性另议”与约束 8。
     - README：BM25 段落与“不支持”清单中的两处 UNIMPLEMENTED；补范围、差异、wheel 体积与内存。
     - AGENTS：“jieba 尚未支持”、module 表、依赖表、命令表。
     - 0020 / 0021：决策 G 的例外。
   - 许可文本随包。
   - 预期计数同步到所有固定点。

每步写完停下评审，评审通过且获得授权后才提交；推送另行授权。

## 测试矩阵

- **组件**：
  - 四处修正各自的反例：扩展区汉字、`鿖`–`鿿`、「B超」与 `total`、发射概率接近的 HMM 输入、`a-b`、`T.S.Eliot`、`1.5%`、`\r\n` 与 Unicode 空白；
  - `mode` 的真值规则；
  - `strip` / `lower` / 停用词：标点保留；单独成词的 `Σ` 得到 `σ`；浮点停用词 `1.5` 命中，`True` / `None` 不命中；
  - 用户词：
    - `suggest_freq` 的前缀条目与已有词；
    - 空串：`total` 约翻倍，之后的切分随之改变，与 Python 一致；
    - 列表中的重复项：每次出现都再加一次；
    - 字符集之外的词只影响 `total`；
    - 超过上限时报错，不 panic。
- **对照**：
  - 固定种子的大批量模糊对照，`cut` 与 `cut_for_search` 都要求逐词相同；
  - 用户词的随机对照：比较 `total`、频率与切分；
  - 固定 fixture 的 `--verify`。
- **引擎**：
  - BM25 写入与检索的稀疏向量、分数与次序（口径与 0031 相同），重开后不变；
  - 共享实例与带用户词的实例并存；
  - 并发写入与检索。
- **差分与互通**：
  - 两条 SDK 线；
  - 3.x 写、本实现读，以及反方向；
  - 带用户词的情形按登记的差异，对两边分别断言。
- **资源**：记入验收记录，不宣称性能提升。
  - 首次加载的耗时与常驻内存；
  - 带用户词的集合数乘以实例大小；
  - 并发写入时的 RSS。
