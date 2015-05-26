#![allow(unused_must_use)]
#![allow(non_upper_case_globals)]

use std::collections::{HashMap, HashSet};
use std::collections::hash_map;
use std::cell::RefCell;
use std::ops::Deref;
use std::rc::Rc;
use std::path::Path;

use syntax::abi;

use types as il;
use types::*;
use clang as cx;
use clang::{ast_dump, Cursor, Diagnostic, TranslationUnit, type_to_str, kind_to_str};
use clangll::*;

use super::Logger;

pub struct ClangParserOptions {
    pub builtin_names: HashSet<String>,
    pub builtins: bool,
    pub match_pat: Vec<String>,
    pub emit_ast: bool,
    pub fail_on_unknown_type: bool,
    pub override_enum_ty: Option<il::IKind>,
    pub clang_args: Vec<String>,
}

struct ClangParserCtx<'a> {
    options: ClangParserOptions,
    name: HashMap<Cursor, Global>,
    globals: Vec<Global>,
    builtin_defs: Vec<Cursor>,
    logger: &'a (Logger+'a),
    err_count: i32
}

fn match_pattern(ctx: &mut ClangParserCtx, cursor: &Cursor) -> bool {
    let (file, _, _, _) = cursor.location().location();

    if file.is_null() {
        return ctx.options.builtins;
    }

    if ctx.options.match_pat.is_empty() {
        return true;
    }

    let name = file.name();
    let mut found = false;
    ctx.options.match_pat.iter().all(|pat| {
        if (&name).contains(pat) {
            found = true;
        }
        true
    });

    return found;
}

fn decl_name(ctx: &mut ClangParserCtx, cursor: &Cursor) -> Global {
    let cursor = cursor.canonical();
    let override_enum_ty = ctx.options.override_enum_ty;
    let new_decl = !ctx.name.contains_key(&cursor);

    let decl = if new_decl {
        let spelling = cursor.spelling();
        let (file, _, _, _) = cursor.location().location();
        let ty = cursor.cur_type();
        let layout = Layout::new(ty.size(), ty.align());
        let filename = match Path::new(&file.name()).file_name() {
            Some(name) => name.to_string_lossy().replace(".", "_"),
            _ => "".to_string()
        };
        let glob_decl = match cursor.kind() {
            CXCursor_StructDecl => {
                let ci = Rc::new(RefCell::new(CompInfo::new(spelling, filename, CompKind::Struct, vec!(), layout)));
                GCompDecl(ci)
            }
            CXCursor_UnionDecl => {
                let ci = Rc::new(RefCell::new(CompInfo::new(spelling, filename, CompKind::Union, vec!(), layout)));
                GCompDecl(ci)
            }
            CXCursor_EnumDecl => {
                let kind = match override_enum_ty {
                    Some(t) => t,
                    None => match cursor.enum_type().kind() {
                        CXType_SChar | CXType_Char_S => ISChar,
                        CXType_UChar | CXType_Char_U => IUChar,
                        CXType_UShort => IUShort,
                        CXType_UInt => IUInt,
                        CXType_ULong => IULong,
                        CXType_ULongLong => IULongLong,
                        CXType_Short => IShort,
                        CXType_Int => IInt,
                        CXType_Long => ILong,
                        CXType_LongLong => ILongLong,
                        _ => IInt,
                    }
                };
                let ei = Rc::new(RefCell::new(EnumInfo::new(spelling, filename, kind, vec!(), layout)));
                GEnumDecl(ei)
            }
            CXCursor_ClassTemplate => {
                let ci = Rc::new(RefCell::new(CompInfo::new(spelling, filename, CompKind::Struct, vec!(), layout)));
                GCompDecl(ci)
            }
            CXCursor_ClassDecl => {
                let args = match ty.num_template_args() {
                    -1 => vec!(),
                    len => {
                        let mut list = Vec::with_capacity(len as usize);
                        for i in 0..len {
                            let arg_type = ty.template_arg_type(i);
                            list.push(conv_ty(ctx, &arg_type, &cursor));
                        }
                        list
                    }
                };
                let mut ci = Rc::new(RefCell::new(CompInfo::new(spelling, filename, CompKind::Struct, vec!(), layout)));
                ci.borrow_mut().args = args;
                GCompDecl(ci)
            }
            CXCursor_TypedefDecl => {
                let ti = Rc::new(RefCell::new(TypeInfo::new(spelling, TVoid, layout)));
                GType(ti)
            }
            CXCursor_VarDecl => {
                let mangled = cursor.mangling();
                let vi = Rc::new(RefCell::new(VarInfo::new(spelling, mangled, TVoid)));
                GVar(vi)
            }
            CXCursor_FunctionDecl => {
                let mangled = cursor.mangling();
                let vi = Rc::new(RefCell::new(VarInfo::new(spelling, mangled, TVoid)));
                GFunc(vi)
            }
            _ => GOther,
        };

        ctx.name.insert(cursor, glob_decl.clone());
        glob_decl
    } else {
        ctx.name.get(&cursor).unwrap().clone()
    };

    if new_decl {
        if ctx.options.builtin_names.contains(&cursor.spelling()) {
            ctx.builtin_defs.push(cursor);
        }
    }

    return decl;
}

fn opaque_decl(ctx: &mut ClangParserCtx, decl: &Cursor) {
    let name = decl_name(ctx, decl);
    ctx.globals.push(name);
}

fn fwd_decl<F:FnOnce(&mut ClangParserCtx)->()>(ctx: &mut ClangParserCtx, cursor: &Cursor, f: F) {
    let def = &cursor.definition();
    if cursor == def {
        f(ctx);
    } else if def.kind() == CXCursor_NoDeclFound ||
              def.kind() == CXCursor_InvalidFile {
        opaque_decl(ctx, cursor);
    }
}

fn get_abi(cc: Enum_CXCallingConv) -> abi::Abi {
    match cc {
        CXCallingConv_Default => abi::C,
        CXCallingConv_C => abi::C,
        CXCallingConv_X86StdCall => abi::Stdcall,
        CXCallingConv_X86FastCall => abi::Fastcall,
        CXCallingConv_AAPCS => abi::Aapcs,
        CXCallingConv_X86_64Win64 => abi::Win64,
        _other => panic!("unsupported calling convention: {}", _other),
    }
}

fn conv_ptr_ty(ctx: &mut ClangParserCtx, ty: &cx::Type, cursor: &Cursor, layout: Layout) -> il::Type {
    let is_const = ty.is_const();
    match ty.kind() {
        CXType_Void => {
            return TPtr(Box::new(TVoid), is_const, layout)
        }
        CXType_Unexposed |
        CXType_FunctionProto |
        CXType_FunctionNoProto => {
            let ret_ty = ty.ret_type();
            let decl = ty.declaration();
            return if ret_ty.kind() != CXType_Invalid {
                TFuncPtr(mk_fn_sig(ctx, ty, cursor))
            } else if cursor.kind() == CXCursor_VarDecl {
                let can_ty = ty.canonical_type();
                conv_ty(ctx, &can_ty, cursor)
            } else {
                TPtr(Box::new(conv_decl_ty(ctx, ty)), ty.is_const(), layout)
            };
        }
        CXType_Typedef => {
            let decl = ty.declaration();
            let def_ty = decl.typedef_type();
            if def_ty.kind() == CXType_FunctionProto ||
               def_ty.kind() == CXType_FunctionNoProto {
                return TPtr(Box::new(conv_ptr_ty(ctx, &def_ty, cursor, layout)), is_const, layout);
            } else {
                return TPtr(Box::new(conv_ty(ctx, ty, cursor)), is_const, layout);
            }
        }
        _ => return TPtr(Box::new(conv_ty(ctx, ty, cursor)), is_const, layout),
    }
}

fn mk_fn_sig(ctx: &mut ClangParserCtx, ty: &cx::Type, cursor: &Cursor) -> il::FuncSig {
    let args_lst: Vec<(String, il::Type)> = match cursor.kind() {
        CXCursor_FunctionDecl | CXCursor_CXXMethod => {
            // For CXCursor_FunctionDecl, cursor.args() is the reliable way to
            // get parameter names and types.
            cursor.args().iter().map(|arg| {
                let arg_name = arg.spelling();
                (arg_name, conv_ty(ctx, &arg.cur_type(), arg))
            }).collect()
        }
        _ => {
            // For non-CXCursor_FunctionDecl, visiting the cursor's children is
            // the only reliable way to get parameter names.
            let mut args_lst = vec!();
            cursor.visit(|c: &Cursor, _: &Cursor| {
                if c.kind() == CXCursor_ParmDecl {
                    args_lst.push((c.spelling(), conv_ty(ctx, &c.cur_type(), c)));
                }
                CXChildVisit_Continue
            });
            args_lst
        }
    };

    let ret_ty = Box::new(conv_ty(ctx, &ty.ret_type(), cursor));
    let abi = get_abi(ty.call_conv());

    il::FuncSig {
        ret_ty: ret_ty,
        args: args_lst,
        is_variadic: ty.is_variadic(),
        abi: abi,
    }
}

fn conv_decl_ty(ctx: &mut ClangParserCtx, ty: &cx::Type) -> il::Type {
    let ty_decl = &ty.declaration();
    return match ty_decl.kind() {
        CXCursor_StructDecl |
        CXCursor_UnionDecl |
        CXCursor_ClassTemplate |
        CXCursor_ClassDecl => {
            let decl = decl_name(ctx, ty_decl);
            let ci = decl.compinfo();
            TComp(ci)
        }
        CXCursor_EnumDecl => {
            let decl = decl_name(ctx, ty_decl);
            let ei = decl.enuminfo();
            TEnum(ei)
        }
        CXCursor_TypedefDecl => {
            let decl = decl_name(ctx, ty_decl);
            let ti = decl.typeinfo();
            TNamed(ti)
        }
        CXCursor_NoDeclFound | CXCursor_TypeAliasDecl => {
            let layout = Layout::new(ty.size(), ty.align());
            TNamed(Rc::new(RefCell::new(TypeInfo::new(ty.spelling().replace("const ", ""), TVoid, layout))))
        }
        _ => {
            let fail = ctx.options.fail_on_unknown_type;
            log_err_warn(ctx,
                &format!("unsupported decl `{}` ({})",
                    kind_to_str(ty_decl.kind()), ty_decl.location()
                ),
                fail
            );
            TVoid
        }
    };
}

fn conv_ty(ctx: &mut ClangParserCtx, ty: &cx::Type, cursor: &Cursor) -> il::Type {
    debug!("conv_ty: ty=`{}` sp=`{}` loc=`{}`", type_to_str(ty.kind()), cursor.spelling(), cursor.location());

    let layout = Layout::new(ty.size(), ty.align());
    return match ty.kind() {
        CXType_Void | CXType_Invalid => TVoid,
        CXType_Bool => TInt(IBool, layout),
        CXType_SChar |
        CXType_Char_S => TInt(ISChar, layout),
        CXType_UChar |
        CXType_Char_U => TInt(IUChar, layout),
        CXType_WChar |
        CXType_Char16 => TInt(IShort, layout),
        CXType_UShort => TInt(IUShort, layout),
        CXType_UInt => TInt(IUInt, layout),
        CXType_ULong => TInt(IULong, layout),
        CXType_ULongLong => TInt(IULongLong, layout),
        CXType_Short => TInt(IShort, layout),
        CXType_Int => TInt(IInt, layout),
        CXType_Long => TInt(ILong, layout),
        CXType_LongLong => TInt(ILongLong, layout),
        CXType_Float => TFloat(FFloat, layout),
        CXType_Double => TFloat(FDouble, layout),
        CXType_LongDouble => TFloat(FDouble, layout),
        CXType_Pointer | CXType_LValueReference => conv_ptr_ty(ctx, &ty.pointee_type(), cursor, layout),
        CXType_VariableArray | CXType_DependentSizedArray | CXType_IncompleteArray => {
            conv_ptr_ty(ctx, &ty.elem_type(), cursor, layout)
        }
        CXType_FunctionProto => TFuncProto(mk_fn_sig(ctx, ty, cursor)),
        CXType_Record |
        CXType_Typedef  |
        CXType_Unexposed |
        CXType_Enum => conv_decl_ty(ctx, ty),
        CXType_ConstantArray => TArray(Box::new(conv_ty(ctx, &ty.elem_type(), cursor)), ty.array_size(), layout),
        _ => {
            let fail = ctx.options.fail_on_unknown_type;
            log_err_warn(ctx,
                &format!("unsupported type `{}` ({})",
                    type_to_str(ty.kind()), cursor.location()
                ),
                fail
            );
            TVoid
        },
    };
}

fn opaque_ty(ctx: &mut ClangParserCtx, ty: &cx::Type) {
    if ty.kind() == CXType_Record || ty.kind() == CXType_Enum {
        let decl = ty.declaration();
        let def = decl.definition();
        if def.kind() == CXCursor_NoDeclFound ||
           def.kind() == CXCursor_InvalidFile {
            opaque_decl(ctx, &decl);
        }
    }
}

/// Recursively visits a cursor that represents a composite (struct or union)
/// type and fills members with CompMember instances representing the fields and
/// nested composites that make up the visited composite.
fn visit_composite(cursor: &Cursor, parent: &Cursor,
                   ctx: &mut ClangParserCtx,
                   ci: &mut CompInfo) -> Enum_CXVisitorResult {

    fn is_bitfield_continuation(field: &il::FieldInfo, ty: &il::Type, width: u32) -> bool {
        match (&field.bitfields, ty) {
            (&Some(ref bitfields), &il::TInt(_, layout)) if *ty == field.ty => {
                let iter = bitfields.iter().map(|&(_, w)| w);
                iter.sum::<u32>() + width <= (layout.size * 8) as u32
            },
            (&Some(ref bitfields), &il::TNamed(ref info)) if *ty == field.ty => {
                let info = info.borrow();
                let iter = bitfields.iter().map(|&(_, w)| w);
                iter.sum::<u32>() + width <= (info.layout.size * 8) as u32
            },
            _ => false
        }
    }

    match cursor.kind() {
        CXCursor_FieldDecl => {
            let ty = conv_ty(ctx, &cursor.cur_type(), cursor);

            let (name, bitfields) = match (cursor.bit_width(), ci.members.last_mut()) {
                // The field is a continuation of an exising bitfield
                (Some(width), Some(&mut il::CompMember::Field(ref mut field)))
                    if is_bitfield_continuation(field, &ty, width) => {

                    if let Some(ref mut bitfields) = field.bitfields {
                        bitfields.push((cursor.spelling(), width));
                    } else { unreachable!() }
                    return CXChildVisit_Continue;
                },
                // The field is the start of a new bitfield
                (Some(width), _) => {
                    // Bitfields containing enums are not supported by the c standard
                    // https://stackoverflow.com/questions/11983231/is-it-safe-to-use-an-enum-in-a-bit-field
                    match &ty {
                        &il::TInt(_, _) => (),
                        _ => {
                            let msg = format!("Enums in bitfields are not supported ({}.{}).",
                                cursor.spelling(), parent.spelling());
                            ctx.logger.warn(&msg);
                        }
                    }
                    ("".to_string(), Some(vec!((cursor.spelling(), width))))
                },
                // The field is not a bitfield
                (None, _) => (cursor.spelling(), None)
            };

            // The Clang C api does not fully expose composite fields, but it
            // does expose them in a way that can be detected. When the current
            // field kind is TComp, TPtr or TArray and the previous member is a
            // composite type - the same type as this field - then this is a
            // composite field.  e.g.:
            //
            //     struct foo {
            //         union {
            //             int a;
            //             char b;
            //         } bar;
            //     };
            //
            //     struct foo {
            //         union {
            //             int a;
            //             char b;
            //         } **bar;
            //     };
            //
            //     struct foo {
            //         union {
            //             int a;
            //             char b;
            //         } bar[3][2];
            //     };
            //

            fn inner_composite(mut ty: &il::Type) -> Option<&Rc<RefCell<CompInfo>>> {
                loop {
                    match ty {
                        &TComp(ref comp_ty) => return Some(comp_ty),
                        &TPtr(ref ptr_ty, _, _) => ty = &**ptr_ty,
                        &TArray(ref array_ty, _, _) => ty = &**array_ty,
                        _ => return None
                    }
                }
            }

            let is_composite = match (inner_composite(&ty), ci.members.last()) {
                (Some(ty_compinfo), Some(&CompMember::Comp(ref c))) => {
                    c.borrow().deref() as *const _ == ty_compinfo.borrow().deref() as *const _
                },
                _ => false
            };

            let field = FieldInfo::new(name, ty.clone(), bitfields);
            if is_composite {
                if let Some(CompMember::Comp(c)) = ci.members.pop() {
                    ci.members.push(CompMember::CompField(c, field));
                } else {
                    unreachable!(); // Checks in is_composite make this unreachable.
                }
            } else {
                ci.members.push(CompMember::Field(field));
            }
        }
        CXCursor_StructDecl | CXCursor_UnionDecl => {
            fwd_decl(ctx, cursor, |ctx_| {
                // If the struct is anonymous (i.e. declared here) then it
                // cannot be used elsewhere and so does not need to be added
                // to globals otherwise it will be declared later and a global.
                let decl = decl_name(ctx_, cursor);
                let ci2 = decl.compinfo();
                cursor.visit(|c, p| {
                    let mut ci_ = ci2.borrow_mut();
                    visit_composite(c, p, ctx_, &mut ci_)
                });
                ci.members.push(CompMember::Comp(decl.compinfo()));
            });
        }
        CXCursor_TemplateTypeParameter => {
            let ty = conv_ty(ctx, &cursor.cur_type(), cursor);
            let layout = Layout::new(ty.size(), ty.align());
            ci.args.push(TNamed(Rc::new(RefCell::new(TypeInfo::new(cursor.spelling(), TVoid, layout)))));
        }
        CXCursor_EnumDecl => {
            fwd_decl(ctx, cursor, |ctx_| {
                let decl = decl_name(ctx_, cursor);
                let ei = decl.enuminfo();
                cursor.visit(|c, _: &Cursor| {
                    let mut ei_ = ei.borrow_mut();
                    visit_enum(c, &mut ei_.items)
                });
                ci.members.push(CompMember::Enum(ei));
            });
        }
        CXCursor_CXXBaseSpecifier => {
            let ty = conv_ty(ctx, &cursor.cur_type(), cursor);
            let fieldname = if ci.members.len() > 0 {
                format!("_base{}", ci.members.len())
            } else {
                "_base".to_string()
            };
            let found_virtual_base = if ci.members.is_empty() {
                false
            } else if let CompMember::Field(ref fi) = ci.members[0] {
                if let TComp(ref ci2) = fi.ty {
                    ci2.borrow().has_vtable
                } else {
                    false
                }
            } else {
                false
            };
            let field = FieldInfo::new(fieldname, ty.clone(), None);
            if !found_virtual_base && cursor.is_virtual_base() {
                ci.members.insert(0, CompMember::Field(field));
                ci.has_vtable = true;
            } else {
                ci.members.push(CompMember::Field(field));
            }
            ci.base_members += 1;
        }
        CXCursor_CXXMethod => {
            let linkage = cursor.linkage();
            if linkage != CXLinkage_External {
                return CXChildVisit_Continue;
            }

            if ci.args.len() > 0 {
                return CXChildVisit_Continue;
            }

            let spelling = cursor.spelling();
            if spelling.len() > 8 &&
               &(spelling)[..8] == "operator" {
                return CXChildVisit_Continue;
            }

            fn is_override(ci: &CompInfo, sig: &Type, name: &str) -> bool {
                for vm in ci.vmethods.iter() {
                    if vm.name == name && &vm.ty == sig {
                        return true;
                    }
                }
                for base in ci.members[..ci.base_members].iter() {
                    let base = match base {
                        &CompMember::Field(ref fi) => {
                            match fi.ty {
                                TComp(ref ci) => ci.clone(),
                                _ => continue,
                            }
                        },
                        _ => unreachable!()
                    };
                    if is_override(&*base.borrow(), sig, name) {
                        return true;
                    }
                }
                return false;
            }

            if cursor.method_is_virtual() {
                ci.has_vtable = true;
            }

            let mut sig = mk_fn_sig(ctx, &cursor.cur_type(), cursor);
            if !cursor.method_is_static() {
                // XXX what have i done
                if cursor.method_is_virtual() {
                    sig.args.insert(0, ("this".to_string(),TPtr(Box::new(TVoid), cursor.cur_type().is_const(), Layout::zero())));
                } else {
                    sig.args.insert(0, ("this".to_string(),
                                        TPtr(Box::new(TNamed(Rc::new(RefCell::new(TypeInfo::new(ci.name.clone(), TVoid, Layout::zero()))))), cursor.cur_type().is_const(), Layout::zero())));
                }
            }

            // XXX with final classes we can optimize a bit
            let sig = TFuncPtr(sig);
            if is_override(ci, &sig, &spelling) {
                return CXChildVisit_Continue;
            }

            let mut vi = VarInfo::new(spelling, cursor.mangling(), sig);
            vi.is_static = cursor.method_is_static();
            vi.is_const = cursor.cur_type().is_const();

            if cursor.method_is_virtual() {
                ci.vmethods.push(vi);
            } else {
                ci.methods.push(vi);
            }
        }
        _ => {
            // XXX: Some kind of warning would be nice, but this produces far
            //      too many.
            //log_err_warn(ctx,
            //    format!("unhandled composite member `{}` (kind {}) in `{}` ({})",
            //        cursor.spelling(), cursor.kind(), parent.spelling(), cursor.location()
            //    ).as_slice(),
            //    false
            //);
        }
    }
    CXChildVisit_Continue
}

fn visit_enum(cursor: &Cursor,
              items: &mut Vec<EnumItem>) -> Enum_CXVisitorResult {
    if cursor.kind() == CXCursor_EnumConstantDecl {
        let name = cursor.spelling();
        let val = cursor.enum_val();
        let item = EnumItem::new(name, val);
        items.push(item);
    }
    return CXChildVisit_Continue;
}

fn visit_top<'r>(cursor: &Cursor,
                 ctx: &mut ClangParserCtx) -> Enum_CXVisitorResult {
    if !match_pattern(ctx, cursor) {
        return CXChildVisit_Continue;
    }

    match cursor.kind() {
        CXCursor_UnexposedDecl => {
            return CXChildVisit_Recurse;
        }
        CXCursor_StructDecl | CXCursor_UnionDecl | CXCursor_ClassDecl | CXCursor_ClassTemplate => {
            fwd_decl(ctx, cursor, |ctx_| {
                let decl = decl_name(ctx_, cursor);
                let ci = decl.compinfo();
                cursor.visit(|c, p| {
                    let mut ci_ = ci.borrow_mut();
                    visit_composite(c, p, ctx_, &mut ci_)
                });
                ctx_.globals.push(GComp(ci));
            });
            return CXChildVisit_Continue;
        }
        CXCursor_EnumDecl => {
            fwd_decl(ctx, cursor, |ctx_| {
                let decl = decl_name(ctx_, cursor);
                let ei = decl.enuminfo();
                cursor.visit(|c, _: &Cursor| {
                    let mut ei_ = ei.borrow_mut();
                    visit_enum(c, &mut ei_.items)
                });
                ctx_.globals.push(GEnum(ei));
            });
            return CXChildVisit_Continue;
        }
        CXCursor_FunctionDecl => {
            let linkage = cursor.linkage();
            if linkage != CXLinkage_External && linkage != CXLinkage_UniqueExternal {
                return CXChildVisit_Continue;
            }

            let spelling = cursor.spelling();
            if spelling.len() > 8 &&
               &(spelling)[..8] == "operator" {
                return CXChildVisit_Continue;
            }

            let func = decl_name(ctx, cursor);
            let vi = func.varinfo();
            let mut vi = vi.borrow_mut();

            vi.ty = TFuncPtr(mk_fn_sig(ctx, &cursor.cur_type(), cursor));
            ctx.globals.push(func);

            return CXChildVisit_Continue;
        }
        CXCursor_VarDecl => {
            let linkage = cursor.linkage();
            if linkage != CXLinkage_External && linkage != CXLinkage_UniqueExternal {
                return CXChildVisit_Continue;
            }

            let ty = conv_ty(ctx, &cursor.cur_type(), cursor);
            let var = decl_name(ctx, cursor);
            let vi = var.varinfo();
            let mut vi = vi.borrow_mut();
            vi.ty = ty.clone();
            vi.is_const = cursor.cur_type().is_const();
            ctx.globals.push(var);

            return CXChildVisit_Continue;
        }
        CXCursor_TypedefDecl => {
            let mut under_ty = cursor.typedef_type();
            if under_ty.kind() == CXType_Unexposed {
                under_ty = under_ty.canonical_type();
            }

            if cursor.spelling() ==
               cursor.typedef_type().declaration().spelling() {
                return CXChildVisit_Continue;
            }
            let ty = conv_ty(ctx, &under_ty, cursor);
            let typedef = decl_name(ctx, cursor);
            let ti = typedef.typeinfo();
            let mut ti = ti.borrow_mut();
            ti.ty = ty.clone();
            ctx.globals.push(typedef);

            opaque_ty(ctx, &under_ty);

            return CXChildVisit_Continue;
        }
        CXCursor_FieldDecl => {
            return CXChildVisit_Continue;
        }
        CXCursor_Namespace => {
            return CXChildVisit_Recurse;
        }
        _ => return CXChildVisit_Continue,
    }
}

fn log_err_warn(ctx: &mut ClangParserCtx, msg: &str, is_err: bool) {
    match is_err {
        true => {
            ctx.err_count += 1;
            ctx.logger.error(msg)
        },
        false => ctx.logger.warn(msg)
    }
}

pub fn parse(options: ClangParserOptions, logger: &Logger) -> Result<Vec<Global>, ()> {
    let mut ctx = ClangParserCtx {
        options: options,
        name: HashMap::new(),
        builtin_defs: vec!(),
        globals: vec!(),
        logger: logger,
        err_count: 0
    };

    let ix = cx::Index::create(false, true);
    if ix.is_null() {
        ctx.logger.error("Clang failed to create index");
        return Err(())
    }

    let unit = TranslationUnit::parse(&ix, "", ctx.options.clang_args.as_slice(), &[], 0);
    if unit.is_null() {
        ctx.logger.error("No input files given");
        return Err(())
    }

    let diags = unit.diags();
    for d in diags.iter() {
        let msg = d.format(Diagnostic::default_opts());
        let is_err = d.severity() >= CXDiagnostic_Error;
        log_err_warn(&mut ctx, &msg, is_err);
    }

    if ctx.err_count > 0 {
        return Err(())
    }

    let cursor = unit.cursor();

    if ctx.options.emit_ast {
        cursor.visit(|cur, _: &Cursor| ast_dump(cur, 0));
    }

    cursor.visit(|cur, _: &Cursor| visit_top(cur, &mut ctx));

    while !ctx.builtin_defs.is_empty() {
        let c = ctx.builtin_defs.remove(0);
        visit_top(&c.definition(), &mut ctx);
    }

    unit.dispose();
    ix.dispose();

    if ctx.err_count > 0 {
        return Err(())
    }

    Ok(ctx.globals)
}
