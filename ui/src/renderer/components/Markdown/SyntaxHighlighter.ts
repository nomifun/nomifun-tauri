import SyntaxHighlighter from 'react-syntax-highlighter/dist/esm/light';
import bash from 'react-syntax-highlighter/dist/esm/languages/hljs/bash';
import c from 'react-syntax-highlighter/dist/esm/languages/hljs/c';
import cpp from 'react-syntax-highlighter/dist/esm/languages/hljs/cpp';
import csharp from 'react-syntax-highlighter/dist/esm/languages/hljs/csharp';
import css from 'react-syntax-highlighter/dist/esm/languages/hljs/css';
import diff from 'react-syntax-highlighter/dist/esm/languages/hljs/diff';
import dockerfile from 'react-syntax-highlighter/dist/esm/languages/hljs/dockerfile';
import go from 'react-syntax-highlighter/dist/esm/languages/hljs/go';
import ini from 'react-syntax-highlighter/dist/esm/languages/hljs/ini';
import java from 'react-syntax-highlighter/dist/esm/languages/hljs/java';
import javascript from 'react-syntax-highlighter/dist/esm/languages/hljs/javascript';
import json from 'react-syntax-highlighter/dist/esm/languages/hljs/json';
import kotlin from 'react-syntax-highlighter/dist/esm/languages/hljs/kotlin';
import latex from 'react-syntax-highlighter/dist/esm/languages/hljs/latex';
import lua from 'react-syntax-highlighter/dist/esm/languages/hljs/lua';
import makefile from 'react-syntax-highlighter/dist/esm/languages/hljs/makefile';
import markdown from 'react-syntax-highlighter/dist/esm/languages/hljs/markdown';
import php from 'react-syntax-highlighter/dist/esm/languages/hljs/php';
import plaintext from 'react-syntax-highlighter/dist/esm/languages/hljs/plaintext';
import powershell from 'react-syntax-highlighter/dist/esm/languages/hljs/powershell';
import python from 'react-syntax-highlighter/dist/esm/languages/hljs/python';
import ruby from 'react-syntax-highlighter/dist/esm/languages/hljs/ruby';
import rust from 'react-syntax-highlighter/dist/esm/languages/hljs/rust';
import scss from 'react-syntax-highlighter/dist/esm/languages/hljs/scss';
import shell from 'react-syntax-highlighter/dist/esm/languages/hljs/shell';
import sql from 'react-syntax-highlighter/dist/esm/languages/hljs/sql';
import swift from 'react-syntax-highlighter/dist/esm/languages/hljs/swift';
import typescript from 'react-syntax-highlighter/dist/esm/languages/hljs/typescript';
import vbnet from 'react-syntax-highlighter/dist/esm/languages/hljs/vbnet';
import xml from 'react-syntax-highlighter/dist/esm/languages/hljs/xml';
import yaml from 'react-syntax-highlighter/dist/esm/languages/hljs/yaml';

const languages = {
  bash,
  c,
  cpp,
  csharp,
  css,
  diff,
  dockerfile,
  go,
  ini,
  java,
  javascript,
  json,
  kotlin,
  latex,
  lua,
  makefile,
  markdown,
  php,
  plaintext,
  powershell,
  python,
  ruby,
  rust,
  scss,
  shell,
  sql,
  swift,
  typescript,
  vbnet,
  xml,
  yaml,
};

for (const [name, grammar] of Object.entries(languages)) {
  SyntaxHighlighter.registerLanguage(name, grammar);
}

for (const alias of ['text', 'txt', 'plain', 'mermaid']) {
  SyntaxHighlighter.registerLanguage(alias, plaintext);
}

export { default as vs } from 'react-syntax-highlighter/dist/esm/styles/hljs/vs';
export { default as vs2015 } from 'react-syntax-highlighter/dist/esm/styles/hljs/vs2015';
export default SyntaxHighlighter;
