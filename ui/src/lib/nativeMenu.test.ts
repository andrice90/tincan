import { eq, test } from "../test/harness";
import { allowNativeMenu, isTextInputType } from "./nativeMenu";

test("native menu: suppressed on plain UI", () => {
  eq(allowNativeMenu({ editable: false, hasSelection: false }), false);
});

test("native menu: kept in text fields for cut, copy and paste", () => {
  eq(allowNativeMenu({ editable: true, hasSelection: false }), true);
  eq(allowNativeMenu({ editable: true, hasSelection: true }), true);
});

test("native menu: kept while text is selected, so Copy works", () => {
  eq(allowNativeMenu({ editable: false, hasSelection: true }), true);
});

test("native menu: which inputs count as text fields", () => {
  for (const t of ["", "text", "TEXT", "search", "url", "tel", "email", "password", "number"]) eq(isTextInputType(t), true, t);
  for (const t of ["range", "checkbox", "radio", "file", "color", "button", "submit"]) eq(isTextInputType(t), false, t);
});
