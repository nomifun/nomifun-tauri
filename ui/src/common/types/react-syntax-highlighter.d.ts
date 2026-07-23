/**
 * Ambient module shims for `react-syntax-highlighter`, which ships no bundled
 * type declarations (and `@types/react-syntax-highlighter` is not installed).
 * Without these, every importer trips TS7016 ("could not find a declaration
 * file ... implicitly has an 'any' type").
 *
 * The library is used only for its default highlighter component and a style
 * preset object, so loose typings are sufficient here.
 */
declare module 'react-syntax-highlighter' {
  import type { ComponentType, ReactNode } from 'react';

  export interface SyntaxHighlighterProps {
    language?: string;
    style?: Record<string, unknown>;
    children?: ReactNode;
    customStyle?: Record<string, unknown>;
    codeTagProps?: Record<string, unknown>;
    PreTag?: keyof JSX.IntrinsicElements | ComponentType<unknown>;
    CodeTag?: keyof JSX.IntrinsicElements | ComponentType<unknown>;
    wrapLines?: boolean;
    wrapLongLines?: boolean;
    showLineNumbers?: boolean;
    [key: string]: unknown;
  }

  export const Prism: ComponentType<SyntaxHighlighterProps>;
  export const Light: ComponentType<SyntaxHighlighterProps>;
  export const LightAsync: ComponentType<SyntaxHighlighterProps>;
  export const PrismAsync: ComponentType<SyntaxHighlighterProps>;
  export const PrismAsyncLight: ComponentType<SyntaxHighlighterProps>;

  const SyntaxHighlighter: ComponentType<SyntaxHighlighterProps>;
  export default SyntaxHighlighter;
}

declare module 'react-syntax-highlighter/dist/esm/styles/hljs' {
  /** Style preset objects (only the ones imported across the app are named). */
  export const vs: Record<string, Record<string, unknown>>;
  export const vs2015: Record<string, Record<string, unknown>>;
  const styles: Record<string, Record<string, unknown>>;
  export default styles;
}

declare module 'react-syntax-highlighter/dist/esm/light' {
  import type { ComponentType } from 'react';
  import type { SyntaxHighlighterProps } from 'react-syntax-highlighter';

  const SyntaxHighlighter: ComponentType<SyntaxHighlighterProps> & {
    registerLanguage(name: string, grammar: unknown): void;
  };
  export default SyntaxHighlighter;
}

declare module 'react-syntax-highlighter/dist/esm/languages/hljs/*' {
  const grammar: unknown;
  export default grammar;
}

declare module 'react-syntax-highlighter/dist/esm/styles/hljs/vs' {
  const style: Record<string, Record<string, unknown>>;
  export default style;
}

declare module 'react-syntax-highlighter/dist/esm/styles/hljs/vs2015' {
  const style: Record<string, Record<string, unknown>>;
  export default style;
}

declare module 'react-syntax-highlighter/dist/esm/styles/prism' {
  const styles: Record<string, Record<string, unknown>>;
  export default styles;
}
