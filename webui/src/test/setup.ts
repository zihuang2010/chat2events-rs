import "@testing-library/jest-dom/vitest";
import { setPlatformAPI } from "echarts/core";

// jsdom 没有文字排版或伪元素布局；图表像素与实际尺寸由浏览器检查覆盖。
setPlatformAPI({ measureText: (text) => ({ width: Array.from(text).length * 7 }) });
const getComputedStyle = window.getComputedStyle.bind(window);
window.getComputedStyle = (element) => getComputedStyle(element);
