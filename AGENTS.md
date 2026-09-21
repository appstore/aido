# AGENTS.md

## 测试放置约定

- 生产源码文件中**不内嵌** `#[cfg(test)] mod tests { ... }` 测试块；单元测试一律拆分为独立文件。
- 拆分方式：在 `src/foo.rs` 尾部声明 `#[cfg(test)] mod tests;`，测试内容放 `src/foo/tests.rs`（Rust 2018 起文件模块可直接挂同名目录下的子模块文件，无需改造成 `mod.rs`）。测试内用 `use super::*;` 访问被测模块，私有项测试能力不损失。
- 跨模块的集成测试放仓库根 `tests/` 目录。
- CI 在 `test.yml` 的 check job 中拦截 src/ 下任何 `#[cfg(test)]` 门控的内联模块体（存量内嵌测试已全量迁移，勿新增）。
