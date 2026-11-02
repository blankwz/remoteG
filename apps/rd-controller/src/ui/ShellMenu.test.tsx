/**
 * ShellMenu 门禁（P2-③ 新建终端控件统一）：自绘下拉与原生 <details> 的差异
 * 就在键控契约——打开聚焦首项、↑↓/Home/End 移焦、Esc 关闭且焦点回触发钮、
 * 选中即回调并收起。两页共用一份，协议锁在组件级。
 */
// @vitest-environment jsdom
import { describe, expect, it, vi, afterEach } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";

import { ShellMenu } from "./ShellMenu";

const SHELLS = [
  { id: "a.exe", label: "甲" },
  { id: "b.exe", label: "乙" },
  { id: "c.exe", label: "丙" },
];

afterEach(cleanup);

function mount(onPick = vi.fn()) {
  render(<ShellMenu shells={SHELLS} onPick={onPick} />);
  return onPick;
}

describe("ShellMenu 键控协议（P2-③）", () => {
  it("打开前无菜单；点击展开并聚焦首项", () => {
    mount();
    expect(screen.queryByRole("menu")).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "新建终端" }));
    const menu = screen.getByRole("menu", { name: "选择终端 shell" });
    expect(menu).toBeTruthy();
    expect(document.activeElement).toBe(screen.getByText("甲"));
  });

  it("↑↓ 循环移焦，Home/End 跳首尾", () => {
    mount();
    fireEvent.click(screen.getByRole("button", { name: "新建终端" }));
    fireEvent.keyDown(window, { key: "ArrowDown" });
    expect(document.activeElement).toBe(screen.getByText("乙"));
    fireEvent.keyDown(window, { key: "ArrowUp" });
    expect(document.activeElement).toBe(screen.getByText("甲"));
    fireEvent.keyDown(window, { key: "End" });
    expect(document.activeElement).toBe(screen.getByText("丙"));
    fireEvent.keyDown(window, { key: "Home" });
    expect(document.activeElement).toBe(screen.getByText("甲"));
  });

  it("Esc 关闭且焦点回触发钮；点菜单项回调 id 并收起", () => {
    const onPick = mount();
    const trigger = screen.getByRole("button", { name: "新建终端" });
    trigger.focus();
    fireEvent.click(trigger);
    fireEvent.keyDown(window, { key: "Escape" });
    expect(screen.queryByRole("menu")).toBeNull();
    expect(document.activeElement).toBe(trigger);
    fireEvent.click(trigger);
    fireEvent.click(screen.getByText("乙"));
    expect(onPick).toHaveBeenCalledWith("b.exe");
    expect(screen.queryByRole("menu")).toBeNull();
  });
});
