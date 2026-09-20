# 文档测试样本

`sample.doc` / `sample.xls`：真实的旧版二进制 Office 文件（Word 97 二进制格式 / BIFF8 工作表，均为复合文件 CFB 容器，内容只有一行文本 "Office preview 中文文档"）。

旧版容器无法像 ZIP 系格式（docx/pptx/odt）那样在测试代码里拼装，因此以提交文件的形式存放；来自 deepseek-harness 的 office 预览测试样本，仅作转换器（anydoc）的端到端回归用途。替换时保持"真实二进制 + 小体积 + 含非 ASCII 文本"即可，断言见 `src/materialize/document.rs` 与 `tests/materialize.rs` 中引用它们的测试。
