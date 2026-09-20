# 文档测试样本

`sample.doc` / `sample.xls`：真实的旧版二进制 Office 文件（Word 97 二进制格式 / BIFF8 工作表，均为复合文件 CFB 容器，内容只有一行文本 "Office preview 中文文档"）。

`sample.xlsb`：真实的 Excel 二进制工作簿（BIFF12 部件装在 OPC/ZIP 包里，网格为 hello/world 两列、一行 1/2）。BIFF12 没有可用的开源写入器，故以提交文件形式存放。

旧版容器与 BIFF12 均无法像 ZIP 系文本格式（docx/pptx/odt/epub）那样在测试代码里拼装，因此以提交文件的形式存放；doc/xls 来自 deepseek-harness 的 office 预览测试样本，xlsb 由 Excel 导出。三者仅作转换器（anydoc）的端到端回归用途。替换时保持"真实二进制 + 小体积 + 含非 ASCII 文本（或可断言的网格）"即可，断言见 `src/materialize/document.rs` 与 `tests/materialize.rs` 中引用它们的测试。
