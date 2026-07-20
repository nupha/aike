// Type declarations for the "yaml" module
// import { YAML } from "yaml";

type YAMLValue =
  | string
  | number
  | boolean
  | null
  | YAMLValue[]
  | { [key: string]: YAMLValue };

declare module "yaml" {
  export const YAML: {
    /** Parse a YAML string into a JavaScript value. */
    parse(text: string): YAMLValue;

    /** Serialize a JavaScript value into a YAML string. */
    stringify(value: YAMLValue): string;
  };
}
