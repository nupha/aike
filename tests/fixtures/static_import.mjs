// A module with a *static* import: the nested specifier must be resolved
// (and therefore policy-checked) in the importing runtime too.
import { value } from "./import_ok.mjs";

export const staticValue = `${value}-static`;
