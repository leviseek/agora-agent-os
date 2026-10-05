import { createRoot } from 'react-dom/client';
import { MarkdownText } from '../src/MarkdownText';
import { ThemeProvider } from '../src/theme';

const answer = "流程如下：\n\n```mermaid\nflowchart LR\n  A[目标] --> B{需要能力?}\n  B -- 是 --> C[调用能力]\n  B -- 否 --> D[直接回答]\n  C --> D\n```\n\n还有一张时序图：\n\n```mermaid\nsequenceDiagram\n  participant U as 用户\n  participant R as 运行时\n  U->>R: 上传 CSV\n  R-->>U: 271,500\n```\n\n坏图：\n\n```mermaid\nflowchart LR\n  A[未闭合\n```\n\n普通代码块仍然是代码：\n\n```python\nprint(9*6)\n```";

createRoot(document.getElementById('root')!).render(
  <ThemeProvider>
    <div id="answer"><MarkdownText text={answer} /></div>
  </ThemeProvider>,
);
