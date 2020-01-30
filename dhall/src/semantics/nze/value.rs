use std::cell::{Ref, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use crate::error::{TypeError, TypeMessage};
use crate::semantics::Binder;
use crate::semantics::{
    apply_any, normalize_one_layer, normalize_tyexpr_whnf, squash_textlit,
};
use crate::semantics::{type_of_builtin, typecheck, TyExpr, TyExprKind};
use crate::semantics::{BuiltinClosure, NzEnv, NzVar, VarEnv};
use crate::syntax::{
    BinOp, Builtin, Const, ExprKind, Integer, InterpolatedTextContents, Label,
    NaiveDouble, Natural, Span,
};
use crate::{Normalized, NormalizedExpr, ToExprOptions};

/// Stores a possibly unevaluated value. Gets (partially) normalized on-demand, sharing computation
/// automatically. Uses a Rc<RefCell> to share computation.
/// If you compare for equality two `Value`s, then equality will be up to alpha-equivalence
/// (renaming of bound variables) and beta-equivalence (normalization). It will recursively
/// normalize as needed.
#[derive(Clone)]
pub(crate) struct Value(Rc<ValueInternal>);

#[derive(Debug)]
struct ValueInternal {
    form: RefCell<Form>,
    /// This is None if and only if `form` is `Sort` (which doesn't have a type)
    ty: Option<Value>,
    span: Span,
}

/// A potentially un-evaluated expression. Once we get to WHNF we won't modify the form again, as
/// explained in the doc for `ValueKind`.
#[derive(Debug, Clone)]
pub(crate) enum Form {
    /// A totally unnormalized value.
    Thunk(Thunk),
    /// A partially normalized value that may need to go through `normalize_one_layer`.
    PartialExpr(ExprKind<Value, Normalized>),
    /// A value in WHNF.
    WHNF(ValueKind),
}

/// An unevaluated subexpression
#[derive(Debug, Clone)]
pub(crate) struct Thunk {
    env: NzEnv,
    body: TyExpr,
}

/// An unevaluated subexpression that takes an argument.
#[derive(Debug, Clone)]
pub(crate) enum Closure {
    /// Normal closure
    Closure {
        arg_ty: Value,
        env: NzEnv,
        body: TyExpr,
    },
    /// Closure that ignores the argument passed
    ConstantClosure { env: NzEnv, body: TyExpr },
}

/// A text literal with interpolations.
// Invariant: this must not contain interpolations that are themselves TextLits, and contiguous
// text values must be merged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TextLit(Vec<InterpolatedTextContents<Value>>);

/// This represents a value in Weak Head Normal Form (WHNF). This means that the value is
/// normalized up to the first constructor, but subexpressions may not be fully normalized.
/// When all the Values in a ValueKind are in WHNF, and recursively so, then the ValueKind is in
/// Normal Form (NF). This is because WHNF ensures that we have the first constructor of the NF; so
/// if we have the first constructor of the NF at all levels, we actually have the NF.
/// In particular, this means that once we get a `ValueKind`, it can be considered immutable, and
/// we only need to recursively normalize its sub-`Value`s to get to the NF.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ValueKind {
    /// Closures
    LamClosure {
        binder: Binder,
        annot: Value,
        closure: Closure,
    },
    PiClosure {
        binder: Binder,
        annot: Value,
        closure: Closure,
    },
    AppliedBuiltin(BuiltinClosure<Value>),

    Var(NzVar),
    Const(Const),
    BoolLit(bool),
    NaturalLit(Natural),
    IntegerLit(Integer),
    DoubleLit(NaiveDouble),
    EmptyOptionalLit(Value),
    NEOptionalLit(Value),
    // EmptyListLit(t) means `[] : List t`, not `[] : t`
    EmptyListLit(Value),
    NEListLit(Vec<Value>),
    RecordType(HashMap<Label, Value>),
    RecordLit(HashMap<Label, Value>),
    UnionType(HashMap<Label, Option<Value>>),
    // Also keep the type of the uniontype around
    UnionConstructor(Label, HashMap<Label, Option<Value>>, Value),
    // Also keep the type of the uniontype and the constructor around
    UnionLit(Label, Value, HashMap<Label, Option<Value>>, Value, Value),
    TextLit(TextLit),
    Equivalence(Value, Value),
    /// Invariant: evaluation must not be able to progress with `normalize_one_layer`?
    PartialExpr(ExprKind<Value, Normalized>),
}

impl Value {
    fn new(form: Form, ty: Value, span: Span) -> Value {
        ValueInternal::new(form, Some(ty), span).into_value()
    }
    pub(crate) fn const_sort() -> Value {
        ValueInternal::new(
            Form::WHNF(ValueKind::Const(Const::Sort)),
            None,
            Span::Artificial,
        )
        .into_value()
    }
    /// Construct a Value from a completely unnormalized expression.
    pub(crate) fn new_thunk(env: &NzEnv, tye: TyExpr) -> Value {
        ValueInternal::new(
            Form::Thunk(Thunk::new(env, tye.clone())),
            tye.get_type().ok(),
            tye.span().clone(),
        )
        .into_value()
    }
    /// Construct a Value from a partially normalized expression that's not in WHNF.
    pub(crate) fn from_partial_expr(
        e: ExprKind<Value, Normalized>,
        t: Value,
    ) -> Value {
        Value::new(Form::PartialExpr(e), t, Span::Artificial)
    }
    /// Make a Value from a ValueKind
    pub(crate) fn from_kind_and_type(v: ValueKind, t: Value) -> Value {
        Value::new(Form::WHNF(v), t, Span::Artificial)
    }
    pub(crate) fn from_const(c: Const) -> Self {
        let v = ValueKind::Const(c);
        match c {
            Const::Type => {
                Value::from_kind_and_type(v, Value::from_const(Const::Kind))
            }
            Const::Kind => {
                Value::from_kind_and_type(v, Value::from_const(Const::Sort))
            }
            Const::Sort => Value::const_sort(),
        }
    }
    pub(crate) fn from_builtin(b: Builtin) -> Self {
        Self::from_builtin_env(b, &NzEnv::new())
    }
    pub(crate) fn from_builtin_env(b: Builtin, env: &NzEnv) -> Self {
        Value::from_kind_and_type(
            ValueKind::from_builtin_env(b, env.clone()),
            typecheck(&type_of_builtin(b)).unwrap().eval_closed_expr(),
        )
    }

    pub(crate) fn as_const(&self) -> Option<Const> {
        match &*self.kind() {
            ValueKind::Const(c) => Some(*c),
            _ => None,
        }
    }
    pub(crate) fn span(&self) -> Span {
        self.0.span.clone()
    }

    fn as_form(&self) -> Ref<Form> {
        self.0.form.borrow()
    }
    /// This is what you want if you want to pattern-match on the value.
    /// WARNING: drop this ref before normalizing the same value or you will run into BorrowMut
    /// panics.
    pub(crate) fn kind(&self) -> Ref<ValueKind> {
        self.normalize_whnf();
        Ref::map(self.as_form(), |form| match form {
            Form::Thunk(..) | Form::PartialExpr(..) => unreachable!(),
            Form::WHNF(k) => k,
        })
    }

    /// Converts a value back to the corresponding AST expression.
    pub(crate) fn to_expr(&self, opts: ToExprOptions) -> NormalizedExpr {
        if opts.normalize {
            self.normalize_nf();
        }

        self.to_tyexpr_noenv().to_expr(opts)
    }
    pub(crate) fn to_whnf_ignore_type(&self) -> ValueKind {
        self.kind().clone()
    }
    /// Before discarding type information, check that it matches the expected return type.
    pub(crate) fn to_whnf_check_type(&self, ty: &Value) -> ValueKind {
        self.check_type(ty);
        self.to_whnf_ignore_type()
    }

    /// Mutates the contents. If no one else shares this, this avoids a RefCell lock.
    fn mutate_form(&mut self, f: impl FnOnce(&mut Form, &Option<Value>)) {
        match Rc::get_mut(&mut self.0) {
            // Mutate directly if sole owner
            Some(vint) => f(RefCell::get_mut(&mut vint.form), &vint.ty),
            // Otherwise mutate through the refcell
            None => f(&mut self.0.form.borrow_mut(), &self.0.ty),
        }
    }
    /// Normalizes contents to normal form; faster than `normalize_nf` if
    /// no one else shares this.
    pub(crate) fn normalize_mut(&mut self) {
        self.mutate_form(|form, ty| form.normalize_nf(ty))
    }

    pub(crate) fn normalize_whnf(&self) {
        self.0.normalize_whnf()
    }
    pub(crate) fn normalize_nf(&self) {
        self.0.normalize_nf()
    }

    pub(crate) fn app(&self, v: Value) -> Value {
        let body_t = match &*self.get_type_not_sort().kind() {
            ValueKind::PiClosure { annot, closure, .. } => {
                v.check_type(annot);
                closure.apply(v.clone())
            }
            _ => unreachable!("Internal type error"),
        };
        Value::from_kind_and_type(apply_any(self.clone(), v, &body_t), body_t)
    }

    /// In debug mode, panic if the provided type doesn't match the value's type.
    /// Otherwise does nothing.
    pub(crate) fn check_type(&self, _ty: &Value) {
        // TODO: reenable
        // debug_assert_eq!(
        //     Some(ty),
        //     self.get_type().ok().as_ref(),
        //     "Internal type error"
        // );
    }
    pub(crate) fn get_type(&self) -> Result<Value, TypeError> {
        Ok(self.0.get_type()?.clone())
    }
    /// When we know the value isn't `Sort`, this gets the type directly
    pub(crate) fn get_type_not_sort(&self) -> Value {
        self.get_type()
            .expect("Internal type error: value is `Sort` but shouldn't be")
    }

    pub fn to_tyexpr(&self, venv: VarEnv) -> TyExpr {
        let map_uniontype = |kts: &HashMap<Label, Option<Value>>| {
            ExprKind::UnionType(
                kts.iter()
                    .map(|(k, v)| {
                        (k.clone(), v.as_ref().map(|v| v.to_tyexpr(venv)))
                    })
                    .collect(),
            )
        };

        let tye = match &*self.kind() {
            ValueKind::Var(v) => TyExprKind::Var(venv.lookup(v)),
            ValueKind::AppliedBuiltin(closure) => closure.to_tyexprkind(venv),
            self_kind => TyExprKind::Expr(match self_kind {
                ValueKind::Var(..) | ValueKind::AppliedBuiltin(..) => {
                    unreachable!()
                }
                ValueKind::LamClosure {
                    binder,
                    annot,
                    closure,
                } => ExprKind::Lam(
                    binder.to_label(),
                    annot.to_tyexpr(venv),
                    closure.to_tyexpr(venv),
                ),
                ValueKind::PiClosure {
                    binder,
                    annot,
                    closure,
                } => ExprKind::Pi(
                    binder.to_label(),
                    annot.to_tyexpr(venv),
                    closure.to_tyexpr(venv),
                ),
                ValueKind::Const(c) => ExprKind::Const(*c),
                ValueKind::BoolLit(b) => ExprKind::BoolLit(*b),
                ValueKind::NaturalLit(n) => ExprKind::NaturalLit(*n),
                ValueKind::IntegerLit(n) => ExprKind::IntegerLit(*n),
                ValueKind::DoubleLit(n) => ExprKind::DoubleLit(*n),
                ValueKind::EmptyOptionalLit(n) => ExprKind::App(
                    Value::from_builtin(Builtin::OptionalNone).to_tyexpr(venv),
                    n.to_tyexpr(venv),
                ),
                ValueKind::NEOptionalLit(n) => {
                    ExprKind::SomeLit(n.to_tyexpr(venv))
                }
                ValueKind::EmptyListLit(n) => {
                    ExprKind::EmptyListLit(TyExpr::new(
                        TyExprKind::Expr(ExprKind::App(
                            Value::from_builtin(Builtin::List).to_tyexpr(venv),
                            n.to_tyexpr(venv),
                        )),
                        Some(Value::from_const(Const::Type)),
                        Span::Artificial,
                    ))
                }
                ValueKind::NEListLit(elts) => ExprKind::NEListLit(
                    elts.iter().map(|v| v.to_tyexpr(venv)).collect(),
                ),
                ValueKind::TextLit(elts) => ExprKind::TextLit(
                    elts.iter()
                        .map(|t| t.map_ref(|v| v.to_tyexpr(venv)))
                        .collect(),
                ),
                ValueKind::RecordLit(kvs) => ExprKind::RecordLit(
                    kvs.iter()
                        .map(|(k, v)| (k.clone(), v.to_tyexpr(venv)))
                        .collect(),
                ),
                ValueKind::RecordType(kts) => ExprKind::RecordType(
                    kts.iter()
                        .map(|(k, v)| (k.clone(), v.to_tyexpr(venv)))
                        .collect(),
                ),
                ValueKind::UnionType(kts) => map_uniontype(kts),
                ValueKind::UnionConstructor(l, kts, t) => ExprKind::Field(
                    TyExpr::new(
                        TyExprKind::Expr(map_uniontype(kts)),
                        Some(t.clone()),
                        Span::Artificial,
                    ),
                    l.clone(),
                ),
                ValueKind::UnionLit(l, v, kts, uniont, ctort) => ExprKind::App(
                    TyExpr::new(
                        TyExprKind::Expr(ExprKind::Field(
                            TyExpr::new(
                                TyExprKind::Expr(map_uniontype(kts)),
                                Some(uniont.clone()),
                                Span::Artificial,
                            ),
                            l.clone(),
                        )),
                        Some(ctort.clone()),
                        Span::Artificial,
                    ),
                    v.to_tyexpr(venv),
                ),
                ValueKind::Equivalence(x, y) => ExprKind::BinOp(
                    BinOp::Equivalence,
                    x.to_tyexpr(venv),
                    y.to_tyexpr(venv),
                ),
                ValueKind::PartialExpr(e) => e.map_ref(|v| v.to_tyexpr(venv)),
            }),
        };

        TyExpr::new(tye, self.0.ty.clone(), self.0.span.clone())
    }
    pub fn to_tyexpr_noenv(&self) -> TyExpr {
        self.to_tyexpr(VarEnv::new())
    }
}

impl ValueInternal {
    fn new(form: Form, ty: Option<Value>, span: Span) -> Self {
        ValueInternal {
            form: RefCell::new(form),
            ty,
            span,
        }
    }
    fn into_value(self) -> Value {
        Value(Rc::new(self))
    }

    fn normalize_whnf(&self) {
        if !self.form.borrow().is_whnf() {
            self.form.borrow_mut().normalize_whnf(&self.ty)
        }
    }
    fn normalize_nf(&self) {
        self.form.borrow_mut().normalize_nf(&self.ty)
    }

    fn get_type(&self) -> Result<&Value, TypeError> {
        match &self.ty {
            Some(t) => Ok(t),
            None => Err(TypeError::new(TypeMessage::Sort)),
        }
    }
}

impl Form {
    fn is_whnf(&self) -> bool {
        match self {
            Form::Thunk(..) | Form::PartialExpr(..) => false,
            Form::WHNF(..) => true,
        }
    }
    fn normalize_whnf(&mut self, ty: &Option<Value>) {
        use std::mem::replace;
        let dummy = Form::PartialExpr(ExprKind::Const(Const::Type));
        *self = match replace(self, dummy) {
            Form::Thunk(th) => Form::WHNF(th.eval()),
            Form::PartialExpr(e) => {
                Form::WHNF(match ty {
                    // TODO: env
                    Some(ty) => normalize_one_layer(e, &ty, &NzEnv::new()),
                    // `value` is `Sort`
                    None => ValueKind::Const(Const::Sort),
                })
            }
            // Already in WHNF
            form @ Form::WHNF(_) => form,
        };
    }
    fn normalize_nf(&mut self, ty: &Option<Value>) {
        if !self.is_whnf() {
            self.normalize_whnf(ty);
        }
        match self {
            Form::Thunk(..) | Form::PartialExpr(_) => unreachable!(),
            Form::WHNF(k) => k.normalize_mut(),
        }
    }
}

impl ValueKind {
    pub(crate) fn into_value_with_type(self, t: Value) -> Value {
        Value::from_kind_and_type(self, t)
    }

    pub(crate) fn normalize_mut(&mut self) {
        match self {
            ValueKind::Var(..)
            | ValueKind::Const(_)
            | ValueKind::BoolLit(_)
            | ValueKind::NaturalLit(_)
            | ValueKind::IntegerLit(_)
            | ValueKind::DoubleLit(_) => {}

            ValueKind::EmptyOptionalLit(tth) | ValueKind::EmptyListLit(tth) => {
                tth.normalize_mut();
            }

            ValueKind::NEOptionalLit(th) => {
                th.normalize_mut();
            }
            ValueKind::LamClosure { annot, closure, .. }
            | ValueKind::PiClosure { annot, closure, .. } => {
                annot.normalize_mut();
                closure.normalize_mut();
            }
            ValueKind::AppliedBuiltin(closure) => closure.normalize_mut(),
            ValueKind::NEListLit(elts) => {
                for x in elts.iter_mut() {
                    x.normalize_mut();
                }
            }
            ValueKind::RecordLit(kvs) => {
                for x in kvs.values_mut() {
                    x.normalize_mut();
                }
            }
            ValueKind::RecordType(kvs) => {
                for x in kvs.values_mut() {
                    x.normalize_mut();
                }
            }
            ValueKind::UnionType(kts)
            | ValueKind::UnionConstructor(_, kts, _) => {
                for x in kts.values_mut().flat_map(|opt| opt) {
                    x.normalize_mut();
                }
            }
            ValueKind::UnionLit(_, v, kts, _, _) => {
                v.normalize_mut();
                for x in kts.values_mut().flat_map(|opt| opt) {
                    x.normalize_mut();
                }
            }
            ValueKind::TextLit(tlit) => tlit.normalize_mut(),
            ValueKind::Equivalence(x, y) => {
                x.normalize_mut();
                y.normalize_mut();
            }
            ValueKind::PartialExpr(e) => {
                e.map_mut(Value::normalize_mut);
            }
        }
    }

    pub(crate) fn from_builtin(b: Builtin) -> ValueKind {
        ValueKind::from_builtin_env(b, NzEnv::new())
    }
    pub(crate) fn from_builtin_env(b: Builtin, env: NzEnv) -> ValueKind {
        ValueKind::AppliedBuiltin(BuiltinClosure::new(b, env))
    }
}

impl Thunk {
    pub fn new(env: &NzEnv, body: TyExpr) -> Self {
        Thunk {
            env: env.clone(),
            body,
        }
    }
    pub fn eval(&self) -> ValueKind {
        normalize_tyexpr_whnf(&self.body, &self.env)
    }
}

impl Closure {
    pub fn new(arg_ty: Value, env: &NzEnv, body: TyExpr) -> Self {
        Closure::Closure {
            arg_ty,
            env: env.clone(),
            body,
        }
    }
    /// New closure that ignores its argument
    pub fn new_constant(env: &NzEnv, body: TyExpr) -> Self {
        Closure::ConstantClosure {
            env: env.clone(),
            body,
        }
    }

    pub fn apply(&self, val: Value) -> Value {
        match self {
            Closure::Closure { env, body, .. } => {
                body.eval(&env.insert_value(val))
            }
            Closure::ConstantClosure { env, body, .. } => body.eval(env),
        }
    }
    fn apply_var(&self, var: NzVar) -> Value {
        match self {
            Closure::Closure { arg_ty, .. } => {
                let val = Value::from_kind_and_type(
                    ValueKind::Var(var),
                    arg_ty.clone(),
                );
                self.apply(val)
            }
            Closure::ConstantClosure { env, body, .. } => body.eval(env),
        }
    }

    // TODO: somehow normalize the body. Might require to pass an env.
    pub fn normalize_mut(&mut self) {}
    /// Convert this closure to a TyExpr
    pub fn to_tyexpr(&self, venv: VarEnv) -> TyExpr {
        self.apply_var(NzVar::new(venv.size()))
            .to_tyexpr(venv.insert())
    }
    /// If the closure variable is free in the closure, return Err. Otherwise, return the value
    /// with that free variable remove.
    pub fn remove_binder(&self) -> Result<Value, ()> {
        let v = NzVar::fresh();
        match self {
            Closure::Closure { .. } => {
                // TODO: handle case where variable is used in closure
                // TODO: return information about where the variable is used
                Ok(self.apply_var(v))
            }
            Closure::ConstantClosure { .. } => {
                // Ok: the variable is indeed ignored
                Ok(self.apply_var(v))
            }
        }
    }
}

impl TextLit {
    pub fn new(
        elts: impl Iterator<Item = InterpolatedTextContents<Value>>,
    ) -> Self {
        TextLit(squash_textlit(elts))
    }
    pub fn interpolate(v: Value) -> TextLit {
        TextLit(vec![InterpolatedTextContents::Expr(v)])
    }
    pub fn from_text(s: String) -> TextLit {
        TextLit(vec![InterpolatedTextContents::Text(s)])
    }

    pub fn concat(&self, other: &TextLit) -> TextLit {
        TextLit::new(self.iter().chain(other.iter()).cloned())
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    /// If the literal consists of only one interpolation and not text, return the interpolated
    /// value.
    pub fn as_single_expr(&self) -> Option<&Value> {
        use InterpolatedTextContents::Expr;
        if let [Expr(v)] = self.0.as_slice() {
            Some(v)
        } else {
            None
        }
    }
    /// If there are no interpolations, return the corresponding text value.
    pub fn as_text(&self) -> Option<String> {
        use InterpolatedTextContents::Text;
        if self.is_empty() {
            Some(String::new())
        } else if let [Text(s)] = self.0.as_slice() {
            Some(s.clone())
        } else {
            None
        }
    }
    pub fn iter(
        &self,
    ) -> impl Iterator<Item = &InterpolatedTextContents<Value>> {
        self.0.iter()
    }
    /// Normalize the contained values. This does not break the invariant because we have already
    /// ensured that no contained values normalize to a TextLit.
    pub fn normalize_mut(&mut self) {
        for x in self.0.iter_mut() {
            x.map_mut(Value::normalize_mut);
        }
    }
}

/// Compare two values for equality modulo alpha/beta-equivalence.
// TODO: use Rc comparison to shortcut on identical pointers
impl std::cmp::PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        *self.kind() == *other.kind()
    }
}
impl std::cmp::Eq for Value {}

impl std::cmp::PartialEq for Thunk {
    fn eq(&self, _other: &Self) -> bool {
        unreachable!(
            "Trying to compare thunks but we should only compare WHNFs"
        )
    }
}
impl std::cmp::Eq for Thunk {}

impl std::cmp::PartialEq for Closure {
    fn eq(&self, other: &Self) -> bool {
        let v = NzVar::fresh();
        self.apply_var(v) == other.apply_var(v)
    }
}
impl std::cmp::Eq for Closure {}

impl std::fmt::Debug for Value {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let vint: &ValueInternal = &self.0;
        let mut x = match &*vint.form.borrow() {
            Form::Thunk(th) => {
                let mut x = fmt.debug_struct(&format!("Value@Thunk"));
                x.field("thunk", th);
                x
            }
            Form::PartialExpr(e) => {
                let mut x = fmt.debug_struct(&format!("Value@PartialExpr"));
                x.field("expr", e);
                x
            }
            Form::WHNF(kind) => {
                if let ValueKind::Const(c) = kind {
                    return write!(fmt, "{:?}", c);
                } else {
                    let mut x = fmt.debug_struct(&format!("Value@WHNF"));
                    x.field("kind", kind);
                    x
                }
            }
        };
        if let Some(ty) = vint.ty.as_ref() {
            x.field("type", &ty);
        } else {
            x.field("type", &None::<()>);
        }
        x.finish()
    }
}
