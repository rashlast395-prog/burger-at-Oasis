import type { Metadata } from "next";
import { StaffBoard } from "@/components/staff-board";

export const metadata: Metadata = { title: "Orders · Burger Oasis staff", robots: { index: false } };

export default function StaffPage() {
  return <StaffBoard />;
}
