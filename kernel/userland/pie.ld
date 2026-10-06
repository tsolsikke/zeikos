/* 位置独立のユーザープログラム（ET_DYN）用のリンカスクリプト（2026-10-06）。
 *
 * **番地 0 からリンクする。** ローダーは、像の全体を決まった量だけずらして載せる（kernel/src/userland.rs の
 * ProcessLayout::POSITION_INDEPENDENT_BASE）。
 *
 * **ELF ヘッダとプログラムヘッダの表を、最初の PT_LOAD に入れる**（FILEHDR と PHDRS）。Linux の静的な位置独立の
 * プログラムは、補助ベクタの AT_PHDR から表を読み、自分の載った位置を求める。表が区画に載っていなければ、
 * 渡す番地が無い。PT_PHDR も置く（表の番地を、そこから読む者が居る）。
 *
 * **区画は、権限ごとにページを分ける**（ヘッダは読むだけ、本文は読んで実行、定数は読むだけ、データは読み書き）。
 * ローダーは、権限の違う区画が同じページに載る像を断る。
 *
 * 動的リンクの節（.dynamic ほか）は捨てる。再配置の要らないプログラムだけを、このスクリプトで作る。
 */
ENTRY(_start)

PHDRS
{
  phdr    PT_PHDR PHDRS FLAGS(4);
  headers PT_LOAD FILEHDR PHDRS FLAGS(4);
  text    PT_LOAD FLAGS(5);
  rodata  PT_LOAD FLAGS(4);
  data    PT_LOAD FLAGS(6);
  stack   PT_GNU_STACK FLAGS(6);
}

SECTIONS
{
  . = SIZEOF_HEADERS;
  .pie.headers : { KEEP(*(.pie.headers)) } :headers

  . = ALIGN(4096);
  .text : {
    KEEP(*(.text.prepad))
    *(.text .text.*)
  } :text

  . = ALIGN(4096);
  .rodata : { *(.rodata .rodata.*) } :rodata

  . = ALIGN(4096);
  .data   : { *(.got .got.*) *(.data .data.*) } :data
  .bss    : { *(.bss .bss.*) *(COMMON) } :data

  /DISCARD/ : {
    *(.comment) *(.note.*) *(.eh_frame*)
    *(.dynamic) *(.dynsym) *(.dynstr) *(.hash) *(.gnu.hash)
  }
}
