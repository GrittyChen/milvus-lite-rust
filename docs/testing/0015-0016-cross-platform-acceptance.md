# 0015／0016 跨平台开发验收

状态：复审通过。复审阻断的记录修订与补验均已完成。范围为四个已评审切片的跨平台开发验证，不代表发行候选通过或发布获准。

## 冻结输入与边界

- 日期：2026-10-17。
- 冻结提交：`3e155cbe4e7fa65693c4020e72e7e268527d8f1b`；`git archive` SHA256：`2f07fa8a010dee4eaa9a00fb01cf1b34818226b83d389bdb23d05d590a46553a`。
- 范围：[0015](../decisions/0015-nullable-defaults-and-array.md) A／B、[0016](../decisions/0016-query-iteration-and-range-search.md) A／B；参照 v3.2.1／`43d1257774e629bc9f66873977ab7c320d5bf5a7`。
- 所有产品构建均来自上述归档，Rust 1.98.1、maturin 1.15.0、locked 依赖、目标默认 CPU；未设置 `target-cpu=native`。正式与 fault-injection wheel 分目录构建和安装。
- 保留当前包版本 `0.1.0rc3`，本轮私有开发产物按提交与哈希识别，**不是已经发布的 rc3 发行物**，不得上传或用来替换既有附件。新增能力仍属于 0.2.0 开发线；版本切换、推送、tag 与发布没有执行。
- 新增的联合冒烟探针是验收工具，不属于冻结产品输入；其 SHA256 随实跑输出单独记录。
- 测试工具增量 `test-tools-v1.tar` 的 SHA256 为 `d2478a6c39d577216d63f1ab3894aa2db36322b60fd5bd493c27e6a23281038a`，包含联合冒烟、测试边界隔离和确定性回归；不改变上述产品归档和 wheel。文件清单与哈希随原始证据保存。
- 复审修订的 `test-tools-v2.tar` SHA256 为 `23f4a00e997a24a9b0d9823556e475d00d735538cefabaacf251ed00c3aa867e`；相对 v1 只修改 `tests/conftest.py` 的两行注释，隔离代码、回归测试和联合冒烟均未改动。v1 及其原始结果继续保留，本次补验使用 v2。

## 平台与检查

Linux x86_64 使用原生 AMD EPYC 7702 主机，CentOS 7／内核 3.10.0-1160，在固定 manylinux_2_28 容器内以非 root、绑定 16 核执行；Linux arm64 使用 Docker Desktop VM 的内核 7.0.14-linuxkit、命名卷内部文件系统、非 root、构建并行数 4。两个容器均为 AlmaLinux 8.10／glibc 2.28，数据和临时目录不使用 macOS bind mount。

基础镜像沿用 [0013](../decisions/0013-first-release-platform-and-packaging.md) 固定身份：x86_64 为 `4131ddd7…`（镜像 ID `2dd9abc548ed…`）；arm64 为 `abd1230a…`（镜像 ID `33379df24040…`）。arm64 工具链复用此前已经校验的 Rust 1.98.1 安装；不复用旧 wheel 或测试通过结果。

macOS arm64 使用 Apple M2 Pro／macOS 27.0.1；两个 Mac 目标显式设置最低版本 11.0。macOS x86_64 在同机 Rosetta 下冒烟，不是原生 Intel Mac 证据。

计划实跑：三种标准平台的默认／FI Rust、Clippy、正式／FI 构建与二进制检查；Linux 两种架构的两条 SDK 差分和 Arrow 15／25 互通；安装、正常关闭重开、隔离 FI、LOCK、Linux strace 与 PID 1；Intel Mac 构建、安装和联合功能冒烟。结果以实际完成的各项为准。

## 构建产物身份

所有平台的 sdist 均为 `133473052ec8fba2b7760378e40a714bb8c3242d7b249792e34936a92d929ca6`；正式 wheel：

| 目标 | wheel SHA256 | 原生库 SHA256 |
| --- | --- | --- |
| macOS arm64 | `697a0941115e771229bbf8ca42d17b5dea8e5de2c393a39c276728047a3a3ca8` | `6c35ca6a66b518aee906ff4cf61b0b1a0ac8e5115ea6cd0fed20cfe6d0dfef69` |
| macOS x86_64（Rosetta） | `b77cdfc88ce34b6da836fcd9b3cb6cd9f9fa0ec3b98111625cdda86b7fbb11bd` | `683325766be5e08147d806d95a82f315d7f3a120069ae78ae4c34562692643d6` |
| Linux x86_64（原生） | `d04d6713fc0f048030c607cf63908a7ec1474add3253faa22d008dc9c481a6d6` | `4401eccb7d01d79d7b87048873c1f5e6266174daf5c37e9a31003ca25058fe6d` |
| Linux arm64 | `53491beccd23ba35b730aa8152dc4965debc278c215a2dc2a71f76f9e436a4fd` | `0e39cb2c793e89848a1032f81d5f13cb02a3b924bb0536207d2601bba5e95b0b` |

隔离 FI wheel：macOS arm64 为 `6e159fa0f70028585de96b4efb77e357316f7ca26ed6c704a909eb385f874424`；Linux x86_64 为 `cc758bb3f51c1619ad8e4d24a20c4956586011e9a8c9656c1edb7cba8522818b`；Linux arm64 为 `54de327d6cc94d6c306ad5b42c945bee9ec5eb6157fca5a7311cbdd289b08895`。检查器均确认正式 wheel 不含隔离入口、FI wheel 含隔离入口。

## 结果

下表汇总本轮跨平台验收；复审修订阶段只补跑 SDK 3.0.2 差分及下述缓存反证，其余结果沿用已经保存的实跑证据，未改写为本次重跑。

| 检查 | macOS arm64 | Linux x86_64（原生） | Linux arm64 |
| --- | --- | --- | --- |
| fmt、默认／all-features Clippy（`-D warnings`） | 通过 | 通过 | 通过 |
| Rust workspace | 313 passed | 314 passed | 314 passed |
| core fault-injection | 399 passed | 400 passed | 400 passed |
| grpc all-features | 25 passed | 25 passed | 25 passed |
| CPython 3.10／3.15 × SDK 2.6.17／3.0.2 全套 | 四组各 421 passed | 四组各 421 passed | 四组各 421 passed |
| CPython 3.11–3.14 安装、无 SDK CLI 与 `.db` 生命周期 | 通过 | 通过 | 通过 |
| 正式 wheel 联合功能冒烟 | 四组通过 | 四组通过 | 四组通过 |
| 隔离 FI compaction／信号套件 | 两条 SDK 各 5 passed | 5 passed | 5 passed |
| 两条 SDK 的差分全套 | 各 325 passed | 各 325 passed | 各 325 passed |
| 两条 SDK 的 Arrow 15／25 互通 | 各 184 passed | 各 184 passed | 各 184 passed |
| 跨实现 LOCK、flock 与释放后重开 | — | 通过 | 通过 |
| strace 同步／提交／删除顺序 | — | 85 次成功调用，通过 | 85 次成功调用，通过 |
| PID 1 正常关闭／排空／强杀恢复 | — | 通过 | 通过 |

Linux Rust 套件各比 macOS 多一项，来自既有平台专属权限用例，不是跳过测试。PID 1 两种架构均为退出码 `0/0/137`、恢复行数 `3/4/4`；强杀场景核对非 OOM 和确认写入的值及 `_seq`。

FI 全套包含 [nullable／ARRAY 的 WAL 与 manifest 崩溃恢复](../../crates/core/tests/crash_recovery.rs)；默认套件及 FI 全套还覆盖 ARRAY 转换、三路线空值过滤、iterator 的旧快照和范围边界。联合冒烟的正常关闭重开与这些崩溃注入证据分别计数。

Intel Mac：同一 x86_64 wheel 在 Rosetta 下、CPython 3.10／3.15、SDK 3.0.2 均通过无 SDK CLI、`.db` 生命周期和联合功能冒烟；未执行原生 Intel 或该平台的全套差分／互通。

联合冒烟 SHA256 为 `de73c2cd2c07a65d990a407a3a0913f08ecbf8a9f9c7daec975d3fe9bb8d5486`。真实 3.x 通过相同数据和断言；已发布 rc3 在创建 nullable schema 时按预期报 UNIMPLEMENTED，证明探针会区分新增能力。仓库内目录与已有目录的拒绝检查通过。工具自检（packaging／platform）为 170 passed，增加缓存隔离后再次运行仍为 170 passed；扩展到 benchmarks 的工具自检，两条 SDK 各 230 passed。

## 本轮发现：两条 SDK 的跨用例 schema 缓存污染

原生 x86_64 的初次 2.6.17 差分为 **3 failed／322 passed**。三个失败节点均在 [range 用例](../../tests/differential/test_range_search.py)的 Insert 准备阶段：两次参照端报 `FieldData 'n' has 8 rows, expected 16`，一次 Rust 端报 `FieldData 'n' has 0 rows, expected 20`。原始完整日志和失败目录保留；隔离运行三项通过不作为清除失败的依据。

已分别用实际测试解释器核对 pymilvus 2.6.17 和 3.0.2 的加载路径、`client/cache.py`、`GrpcHandler._get_schema` 及关闭／创建路径：两条 SDK 的 `GlobalCache.schema` 均以 `(endpoint, db_name, collection_name)` 为键；关闭客户端及 CreateCollection 不使它失效。两版本的 `cache.py` SHA256 均为 `7832980e08780fe0ccbd1615e6299290d49a1f67f67ea6551c191b2ad85d8dfb`。此前“3.0.2 没有此缓存”的结论错误，现撤回。

独立数据库服务复用临时端口时，旧的非 nullable schema 可被新客户端沿用。固定端口的两代数据库探针在 Rust 和真实 3.x 上均复现：服务器 Describe 为 nullable，SDK 缓存为非 nullable；16 行请求只携带 8 个实际值、没有 `valid_data`，两端均报 code 6，失败请求均为零写入。在独立测试边界初始化 SDK 缓存后，两端均发出 16 位掩码并成功写入 16 行。最初使用 2.6.17 的四组探针保留，本次另用 3.0.2 在两个服务端各验证污染／清理两种状态，结果相同。原始两端比较因而未满足“同一输入”的差分前提。

最小修复为[函数级 autouse 隔离](../../tests/conftest.py)：仅在独立用例开始时重置已加载的 SDK 全局 schema／timestamp 缓存，两条 SDK 都会执行该分支；进程未加载此模块时不做任何事，不新增 SDK 依赖，不捕获或忽略清理失败。重置后首次依赖 schema 的调用会重新 DescribeCollection，不能据此认为请求与隔离前相同。同一用例内的请求、缓存、关闭重开、Drop／recreate 行为不变；产品、Python 绑定、参照实现和服务端 valid_data 校验均未修改。

[确定性回归](../../tests/python/test_sdk_cache_isolation.py)在模块级准备旧数据库与旧 schema，关闭后由函数级隔离划定新用例边界，再以相同端口打开另一数据库并写入 nullable 列。本次在 macOS arm64 的实际验收 wheel 上分别运行两条 SDK：启用隔离均为 1 passed；使用 `--noconftest` 禁用隔离时均为 1 failed，稳定报 code 6、`FieldData 'n' has 8 rows, expected 16`，断言未放宽。

隔离修复后的既有结果包括三种标准平台的四组 Python 全套（各 421 passed）及 2.6 差分（各 325 passed）。初轮 420 项和修复后的 421 项分别保留日志，后者包含新增回归。此前的 3.0.2 差分各 325 passed 未建立隔离修复后的证据，现仅作历史结果保留；本次三平台补验结果另行记录。两条 SDK 的互通各 184 passed 沿用既有结果：互通套件仅在子进程的代码字符串中使用 SDK，pytest 父进程不加载 SDK 缓存模块，函数级隔离对其不起作用，因此本次不重跑互通；本次另在实际验收环境收集 184 个互通节点后核对父进程 `sys.modules`，确认未加载 `pymilvus` 或其缓存模块。

清扫范围：分别检索两条 SDK 实际安装目录的 `GlobalCache`、`_get_schema`、`_invalidate_schema`、`create_collection`、`close`、`release_server` 和 `valid_data`，覆盖 schema 获取 → 缓存键 → 行编码 → Insert／Upsert → 关闭／重建；测试侧检查 `ReopenPair`、`ServerManager` 和各套件的 fixture 生命周期，并清扫仓库中“3.0 无缓存”的相关表述及旧标题锚点。此前未保存明细的 3.0 差分偶发事件可能同因，但无法确认。

真实应用中，两条 SDK 都有此客户端限制，真实 3.x 也可复现，不属于 Rust 服务端差异。通过 `.db` 使用时，SDK 不会主动调用 `release_server`，普通关闭客户端通常不会释放进程内服务；实际触发条件包括显式释放后另开数据库并复用同一端口，或独立 CLI 服务在固定端口更换数据目录重启，且数据库名、集合名相同而 schema 不同。函数级隔离只阻止跨用例污染，不覆盖同一用例内部的此类复用。本轮不修改 SDK，限制及触发条件集中记录于此。

### 复审后的 3.0.2 完整差分补验

三个平台均使用 CPython 3.12、SDK 3.0.2、v2 隔离 fixture 和上表本轮正式原生库，各一次完整运行，退出码均为 0，无跳过或断言修改。工具在启动前记录实际 SDK 加载路径、缓存源文件、fixture 和原生库哈希；结束后保存完整日志、argv、起止时间及退出码，另由门槛脚本核对计数和身份。

| 平台 | 结果 | 耗时（秒） | 完整差分日志 SHA256 |
| --- | --- | --- | --- |
| macOS arm64 | 325 passed | 696.93 | `a58db715e350c76bc7f685b608d38cf608bf0a54560a28c42197febaed6005a2` |
| Linux x86_64（原生） | 325 passed | 1164.80 | `eff73995b22b923f8fe122beb3782787aed4c3d3efc43e16c3d66d850d223920` |
| Linux arm64 | 325 passed | 790.58 | `c641017348f35c6d78623970cedb2a8b160c6fac743dfe938dea8a1f89d7ac7f` |

## 真值核验

Linux 两种架构均核验 parse 7791 项、dynamic 3570 项，以及 Arrow 15／25 各 4737 项完整布尔树；新增 ARRAY fixture 在两个 Arrow 版本均通过。nullable 的 filters 输出共 510 项，其中固定标量 fixture 的 414 项由 `--verify-nullable-filter-fixture` 核对；Query iterator 10 组数据集／280 项观察、range／V1 15 组数据集／1100 项观察均由 `--verify` 核对。后两项本轮使用 SDK 3.0.2 的参照环境，两条 SDK 的端到端覆盖由差分全套另行计数。

## 证据边界

- 本轮没有产品源码或构建配置改动；原归档的非目录条目共 316 项：314 个普通文件与 2 个软链（`.claude/skills` → `../.agents/skills`、`CLAUDE.md` → `AGENTS.md`）。三个平台均核对 314 个普通文件逐字节一致，其中产品源码及构建输入为 116 个；本次另核对两个软链的类型及目标。测试工具按上述独立清单叠加，新增文件为验收工具和记录。
- 环境准备中断单独留存：arm64 的日志目录权限和依赖下载准备、strace 的软件源访问，以及 x86_64 的 PID 1 驱动和依赖下载准备。均发生在对应测试执行之前；修正环境后使用全新数据目录完成检查，没有改测试断言。strace 使用官方 AlmaLinux RPM 并通过签名／摘要验证；x86_64 PID 1 使用固定 Debian 基础镜像的仅配置包镜像源派生层，实际镜像 ID 另随探针记录。
- macOS 11 真机、原生 Intel Mac、Windows、真实断电未执行；不从 Mach-O 下限字段或容器结果推导这些平台已通过。
- 本轮没有重跑三组兼容性统计、旧 RC 回退矩阵、性能专项、sdist 的 MSRV／PEP 517 回退全套；对应历史证据仍见各切片验收。本轮新增的是跨平台证据，不将历史结果改写为新实测。
- 原始容器日志使用 UTC，记录日期按 Asia/Singapore。

## 复现入口与证据

构建与检查入口见[发行物流程](../../tests/packaging/README.md)，Linux LOCK／strace／PID 1 见[平台流程](../../tests/platform/README.md)，参照环境按[互通 README](../../tests/storage_interop/README.md)分别安装完整引擎和 Arrow 15.0.0／25.0.1；Arrow 15 的 NumPy 固定为 1.26.4。

本次补验从仓库外的全新目录运行，使用上表已安装的正式 wheel、冻结源树叠加 v2 测试工具，不重建产品。完整差分与缓存反证的命令形态如下（反证在两条 SDK 上各执行，正反两次使用不同的新临时目录）：

```bash
MILVUS_LITE_3X_PYTHON=<reference-python> <sdk30-python> -m pytest <frozen-source>/tests/differential -q --basetemp=<new-diff-temp>
<sdk-python> -m pytest <source>/tests/python/test_sdk_cache_isolation.py -q --basetemp=<new-positive-temp>
<sdk-python> -m pytest <source>/tests/python/test_sdk_cache_isolation.py -q --noconftest --basetemp=<new-negative-temp>
```

最后一条预期退出 1，且只因上述 code 6／8 行对 16 行失败；它是隔离机制的反证，不计作套件通过。实际解释器、完整 argv、退出码、SDK 源文件和原生库身份均随本次证据保存。

[联合冒烟](../../tests/platform/feature_smoke.py)在两条 SDK 环境中逐阶段核对 nullable／默认值、ARRAY、Query iterator、range 和 V1；这是安装与生命周期证据，不等同于崩溃恢复。原始日志、源码归档、构建产物、数据目录和容器信息保存在仓库外，按提交归档，不提交到源码库。

三平台原始证据的文件级哈希已全部核验，独立门槛清单逐项核对退出结果、计数、SDK、探针和已安装原生库身份；原始失败日志和失败目录单独保留。原有平台归档及门槛清单按原哈希保留，本次 3.0.2 补验另以修订证据包与新门槛清单保存。证据归档 SHA256：

| 归档 | SHA256 |
| --- | --- |
| macOS | `285de89dd15abbcacd7a2436549e379d88352ac3d3a52b6cc09f01c338b1e9f4` |
| Linux arm64 | `c178f2d3585af48a84b39781ad062e233e67f1dc82aaf1a164afb2b37ddaebe0` |
| Linux x86_64 | `954cb081c022a7ec7cc917935b74432f5092860ed9a138bd15105305851d3cc9` |
| 初轮失败目录 | `46605aeaf7df28d0fc72b61c5feb066df9be0ef34451aca37c097444da238584` |
| 补验前门槛清单（不含本次 3.0.2 补验） | `c193911ad467de83a16921fbc7fd01c459e9494bab9c3491707d2d9bda1849c6` |
| 本次修订与补验证据包 | `6bf0a3cf8a70e616c427fd22fe02767d70bf031674a505a26f147a5613bbbe70` |
| 本次补验门槛清单 | `61c64dfbdccff849ec80d585901a855e822ba659ba3b5ea31ff2182fddd3a7ce` |

本次修订证据包 `review-fix-sdk-cache-evidence.tar.gz` 含 44 个文件，其中逐文件哈希清单覆盖其余 43 项；三份完整差分、两条 SDK 的正反回归、3.0.2 的两服务端缓存探针、实际 SDK 源码摘录、源文件／软链核对及 v2 测试工具均在包内，归档后再次核对全部文件哈希通过；新门槛清单为包内的 `review-gate.json`。

最终候选仍须按 0013 在最终版本与冻结提交上重新构建验收；本轮不代替该门槛。
