/** 导出测试按表头名取格子，不按下标 —— 列序一变，按下标的断言会静默指向别的列。 */
import type { WorkbookSheet } from "@/features/export/workbook";

export const headers = (sheet: WorkbookSheet) => sheet.columns.map((c) => c.header);
/** 某一列在所有数据行里的值。 */
export const column = (sheet: WorkbookSheet, header: string) =>
  sheet.rows.map((r) => r[headers(sheet).indexOf(header)]);
/** 把第 i 个数据行按表头摊成对象，断言只挑关心的列。 */
export const record = (sheet: WorkbookSheet, i: number) =>
  Object.fromEntries(headers(sheet).map((h, j) => [h, sheet.rows[i]![j]]));
