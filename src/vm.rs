use cpu_time::ProcessTime;

use crate::{
    chunk::{Instruction, Value},
    compiler::compile,
    error::LoxError,
    gc::{Gc, GcObject, GcRef},
    objects::{BoundMethod, Class, Closure, Instance, LoxString, NativeFunction, Upvalue},
    table::Table,
};
use std::{
    ops::Deref,
    ptr::{null, null_mut},
};

pub struct Vm {
    gc: Gc,
    frames: [CallFrame; Vm::MAX_FRAMES],
    frame_count: usize,
    stack: [Value; Vm::STACK_SIZE],
    stack_top: *mut Value,
    globals: Table,
    open_upvalues: Vec<GcRef<Upvalue>>,
    init_string: GcRef<LoxString>,
    start_time: ProcessTime,
}

impl Vm {
    const MAX_FRAMES: usize = 64;
    const STACK_SIZE: usize = Vm::MAX_FRAMES * (std::u8::MAX as usize) + 1;

    pub fn new() -> Self {
        let mut gc = Gc::new();
        let init_string = gc.intern("init".to_owned());

        Self {
            gc,
            frames: [CallFrame {
                closure: GcRef::dangling(),
                ip: null(),
                slot: 0,
            }; Vm::MAX_FRAMES],
            frame_count: 0,
            stack: [Value::Nil; Vm::STACK_SIZE],
            stack_top: null_mut(),
            globals: Table::new(),
            open_upvalues: Vec::with_capacity(Vm::STACK_SIZE),
            init_string,
            start_time: ProcessTime::now(),
        }
    }

    pub fn initialize(&mut self) {
        self.define_native("clock", NativeFunction(clock));
        self.define_native("panic", NativeFunction(lox_panic));
        self.stack_top = self.stack.as_mut_ptr();
    }

    pub fn interpret(&mut self, code: &str) -> Result<(), LoxError> {
        let function = compile(code, &mut self.gc)?;
        self.push(Value::Function(function));
        let closure = self.alloc(Closure::new(function));
        self.frames[self.frame_count] = CallFrame::new(closure, 0);
        self.frame_count += 1;
        self.run()
    }

    fn push(&mut self, v: Value) {
        unsafe {
            *self.stack_top = v;
            self.stack_top = self.stack_top.offset(1);
        }
    }

    fn pop(&mut self) -> Value {
        unsafe {
            self.stack_top = self.stack_top.offset(-1);
            *self.stack_top
        }
    }

    fn peek(&self, n: usize) -> Value {
        unsafe { *self.stack_top.offset(-1 - n as isize) }
    }

    fn stack_truncate(&mut self, index: usize) {
        unsafe { self.stack_top = self.stack.as_mut_ptr().add(index) }
    }

    fn stack_len(&self) -> usize {
        unsafe { self.stack_top.offset_from(self.stack.as_ptr()) as usize }
    }

    fn set_at(&mut self, n: usize, value: Value) {
        unsafe {
            let pos = self.stack_top.offset(-1 - (n as isize));
            *pos = value
        }
    }

    fn define_native(&mut self, name: &str, native: NativeFunction) {
        let name = self.gc.intern(name.to_owned());
        self.globals.set(name, Value::NativeFunction(native));
    }

    fn runtime_error(&self, msg: &str) -> Result<(), LoxError> {
        let current_frame = &self.frames[self.frame_count - 1];
        eprintln!("{}", msg);
        eprintln!("[line {}] in script", current_frame.line());
        Err(LoxError::RuntimeError)
    }

    fn binary_op<T>(&mut self, f: fn(f64, f64) -> T, r: fn(T) -> Value) -> Result<(), LoxError> {
        let operands = (self.pop(), self.pop());
        match operands {
            (Value::Number(value_b), Value::Number(value_a)) => {
                self.push(r(f(value_a, value_b)));
                Ok(())
            }
            _ => self.runtime_error("Operands must be numbers."),
        }
    }

    fn run(&mut self) -> Result<(), LoxError> {
        let mut current_frame =
            unsafe { &mut *(&mut self.frames[self.frame_count - 1] as *mut CallFrame) };
        let mut current_chunk = &current_frame.closure.function.chunk;
        loop {
            let instruction = unsafe { *current_frame.ip };

            #[cfg(feature = "debug_trace_execution")]
            {
                let dis = crate::chunk::Disassembler::new(
                    current_chunk,
                    Some(&self.stack[0..self.stack_len()]),
                );
                dis.instruction(&instruction, current_frame.offset());
            }

            current_frame.ip = unsafe { current_frame.ip.offset(1) };

            match instruction {
                Instruction::Add => {
                    let (b, a) = (self.pop(), self.pop());
                    match (&a, &b) {
                        (Value::Number(a), Value::Number(b)) => {
                            self.push(Value::Number(a + b));
                        }

                        (Value::String(a), Value::String(b)) => {
                            let result = format!("{}{}", a.deref(), b.deref());
                            let result = self.intern(result);
                            let value = Value::String(result);
                            self.push(value);
                        }

                        _ => {
                            self.push(a);
                            self.push(b);
                            return self
                                .runtime_error("Operands must be two numbers or two strings.");
                        }
                    }
                }
                Instruction::Class(constant) => {
                    let class_name = current_chunk.read_string(constant);
                    let class = Class::new(class_name);
                    let class = self.alloc(class);
                    self.push(Value::Class(class));
                }
                Instruction::CloseUpvalue => {
                    let stack_top = self.stack_len() - 1;
                    self.close_upvalues(stack_top);
                    self.pop();
                }
                Instruction::Closure(constant) => {
                    let function = current_chunk.read_constant(constant);
                    if let Value::Function(function) = function {
                        let upvalue_count = function.upvalues.len();
                        let mut closure = Closure::new(function);

                        for i in 0..upvalue_count {
                            let upvalue = function.upvalues[i];
                            let obj_upvalue = if upvalue.is_local {
                                let location = current_frame.slot + upvalue.index as usize;
                                self.capture_upvalue(location)
                            } else {
                                current_frame.closure.upvalues[upvalue.index as usize]
                            };
                            closure.upvalues.push(obj_upvalue)
                        }

                        let closure = self.alloc(closure);
                        self.push(Value::Closure(closure));
                    } else {
                        panic!("Closure instruction without function value");
                    }
                }
                Instruction::Call(arg_count) => {
                    self.call_value(arg_count as usize)?;
                    current_frame =
                        unsafe { &mut *(&mut self.frames[self.frame_count - 1] as *mut CallFrame) };
                    current_chunk = &current_frame.closure.function.chunk;
                }
                Instruction::Constant(constant) => {
                    let value = current_chunk.read_constant(constant);
                    self.push(value);
                }
                Instruction::DefineGlobal(constant) => {
                    let global_name = current_chunk.read_string(constant);
                    let value = self.pop();
                    self.globals.set(global_name, value);
                }
                Instruction::Divide => self.binary_op(|a, b| a / b, Value::Number)?,
                Instruction::Equal => {
                    let a = self.pop();
                    let b = self.pop();
                    self.push(Value::Bool(a == b));
                }
                Instruction::False => self.push(Value::Bool(false)),
                Instruction::GetGlobal(constant) => {
                    let global_name = current_chunk.read_string(constant);
                    match self.globals.get(global_name) {
                        Some(value) => self.push(value),
                        None => {
                            let msg = format!("Undefined variable '{}'.", global_name.deref());
                            return self.runtime_error(&msg);
                        }
                    }
                }
                Instruction::GetLocal(slot) => {
                    let i = slot as usize + current_frame.slot;
                    let value = self.stack[i];
                    self.push(value);
                }
                Instruction::GetProperty(constant) => {
                    if let Value::Instance(instance) = self.peek(0) {
                        let class = instance.class;
                        let property_name = current_chunk.read_string(constant);
                        let value = instance.fields.get(property_name);
                        match value {
                            Some(value) => {
                                self.pop();
                                self.push(value);
                            }
                            None => {
                                self.bind_method(class, property_name)?;
                            }
                        }
                    } else {
                        return self.runtime_error("Only instances have properties.");
                    }
                }
                Instruction::GetSuper(constant) => {
                    let method_name = current_chunk.read_string(constant);
                    if let Value::Class(superclass) = self.pop() {
                        self.bind_method(superclass, method_name)?;
                    } else {
                        panic!("super found no class");
                    }
                }
                Instruction::GetUpvalue(slot) => {
                    let value = {
                        let upvalue = current_frame.closure.upvalues[slot as usize];
                        if let Some(value) = upvalue.closed {
                            value
                        } else {
                            self.stack[upvalue.location]
                        }
                    };
                    self.push(value);
                }
                Instruction::Greater => self.binary_op(|a, b| a > b, Value::Bool)?,
                Instruction::Inherit => {
                    let pair = (self.peek(0), self.peek(1));
                    if let (Value::Class(mut subclass), Value::Class(superclass)) = pair {
                        subclass.methods = Table::new();
                        subclass.methods.add_all(&superclass.methods);
                        self.pop();
                    } else {
                        return self.runtime_error("Superclass must be a class.");
                    }
                }
                Instruction::Invoke((constant, arg_count)) => {
                    let name = current_chunk.read_string(constant);
                    self.invoke(name, arg_count as usize)?;
                    current_frame =
                        unsafe { &mut *(&mut self.frames[self.frame_count - 1] as *mut CallFrame) };
                    current_chunk = &current_frame.closure.function.chunk;
                }
                Instruction::Jump(offset) => {
                    current_frame.ip = unsafe { current_frame.ip.offset(offset as isize) };
                }
                Instruction::JumpIfFalse(offset) => {
                    if self.peek(0).is_falsey() {
                        current_frame.ip = unsafe { current_frame.ip.offset(offset as isize) };
                    }
                }
                Instruction::Less => self.binary_op(|a, b| a < b, Value::Bool)?,
                Instruction::Loop(offset) => {
                    current_frame.ip = unsafe { current_frame.ip.offset(-1 - (offset as isize)) };
                }
                Instruction::Method(constant) => {
                    let method_name = current_chunk.read_string(constant);
                    self.define_method(method_name);
                }
                Instruction::Multiply => self.binary_op(|a, b| a * b, Value::Number)?,
                Instruction::Negate => {
                    if let Value::Number(value) = self.peek(0) {
                        self.pop();
                        self.push(Value::Number(-value));
                    } else {
                        return self.runtime_error("Operand must be a number.");
                    }
                }
                Instruction::Nil => self.push(Value::Nil),
                Instruction::Not => {
                    let value = self.pop();
                    self.push(Value::Bool(value.is_falsey()));
                }
                Instruction::Pop => {
                    self.pop();
                }
                Instruction::Print => {
                    println!("{}", self.pop());
                }
                Instruction::Return => {
                    self.frame_count -= 1;
                    let return_value = self.pop();
                    self.close_upvalues(current_frame.slot);

                    if self.frame_count == 0 {
                        return Ok(());
                    } else {
                        self.stack_truncate(current_frame.slot);
                        self.push(return_value);

                        current_frame = unsafe {
                            &mut *(&mut self.frames[self.frame_count - 1] as *mut CallFrame)
                        };
                        current_chunk = &current_frame.closure.function.chunk;
                    }
                }
                Instruction::SetGlobal(constant) => {
                    let global_name = current_chunk.read_string(constant);
                    let value = self.peek(0);
                    if self.globals.set(global_name, value) {
                        self.globals.delete(global_name);
                        let msg = format!("Undefined variable '{}'.", global_name.deref());
                        return self.runtime_error(&msg);
                    }
                }
                Instruction::SetLocal(slot) => {
                    let i = slot as usize + (*current_frame).slot;
                    let value = self.peek(0);
                    self.stack[i] = value;
                }
                Instruction::SetProperty(constant) => {
                    if let Value::Instance(mut instance) = self.peek(1) {
                        let property_name = current_chunk.read_string(constant);
                        let value = self.pop();
                        instance.fields.set(property_name, value);
                        self.pop();
                        self.push(value);
                    } else {
                        return self.runtime_error("Only instances have fields.");
                    }
                }
                Instruction::SetUpvalue(slot) => {
                    let mut upvalue = current_frame.closure.upvalues[slot as usize];
                    let value = self.peek(0);
                    if upvalue.closed.is_none() {
                        self.stack[upvalue.location] = value;
                    } else {
                        upvalue.closed = Some(value);
                    }
                }
                Instruction::Substract => self.binary_op(|a, b| a - b, Value::Number)?,
                Instruction::SuperInvoke((constant, arg_count)) => {
                    let method_name = current_chunk.read_string(constant);
                    if let Value::Class(class) = self.pop() {
                        self.invoke_from_class(class, method_name, arg_count as usize)?;
                        current_frame = unsafe {
                            &mut *(&mut self.frames[self.frame_count - 1] as *mut CallFrame)
                        };
                        current_chunk = &current_frame.closure.function.chunk;
                    } else {
                        panic!("super invoke with no class");
                    }
                }
                Instruction::True => self.push(Value::Bool(true)),
            };
        }
    }
    fn call_value(&mut self, arg_count: usize) -> Result<(), LoxError> {
        let callee = self.peek(arg_count);
        match callee {
            Value::BoundMethod(bound) => {
                let method = bound.method;
                let receiver = bound.receiver;
                self.set_at(arg_count, receiver);
                self.call(method, arg_count)
            }
            Value::Class(class) => {
                let instance = Instance::new(class);
                let instance = self.alloc(instance);
                self.set_at(arg_count, Value::Instance(instance));
                if let Some(initializer) = class.methods.get(self.init_string) {
                    if let Value::Closure(initializer) = initializer {
                        return self.call(initializer, arg_count);
                    }
                    return self.runtime_error("Initializer is not closure");
                } else if arg_count != 0 {
                    let msg = format!("Expected 0 arguments but got {}.", arg_count);
                    return self.runtime_error(&msg);
                }
                Ok(())
            }
            Value::Closure(closure) => self.call(closure, arg_count),
            Value::NativeFunction(native) => {
                let left = self.stack_len() - arg_count;
                let result = native.0(&self, &self.stack[left..]);
                self.stack_truncate(left - 1);
                self.push(result);
                Ok(())
            }
            _ => self.runtime_error("Can only call functions and classes."),
        }
    }

    fn call(&mut self, closure: GcRef<Closure>, arg_count: usize) -> Result<(), LoxError> {
        let function = closure.function;
        if arg_count != function.arity {
            let msg = format!(
                "Expected {} arguments but got {}.",
                function.arity, arg_count
            );
            self.runtime_error(&msg)
        } else if self.frame_count == Vm::MAX_FRAMES {
            self.runtime_error("Stack overflow.")
        } else {
            let frame = CallFrame::new(closure, self.stack_len() - arg_count - 1);
            self.frames[self.frame_count] = frame;
            self.frame_count += 1;
            Ok(())
        }
    }

    fn invoke(&mut self, name: GcRef<LoxString>, arg_count: usize) -> Result<(), LoxError> {
        let receiver = self.peek(arg_count);
        if let Value::Instance(instance) = receiver {
            if let Some(field) = instance.fields.get(name) {
                self.set_at(arg_count, field);
                self.call_value(arg_count)
            } else {
                let class = instance.class;
                self.invoke_from_class(class, name, arg_count)
            }
        } else {
            self.runtime_error("Only instances have methods.")
        }
    }

    fn invoke_from_class(
        &mut self,
        class: GcRef<Class>,
        name: GcRef<LoxString>,
        arg_count: usize,
    ) -> Result<(), LoxError> {
        if let Some(method) = class.methods.get(name) {
            if let Value::Closure(closure) = method {
                self.call(closure, arg_count)
            } else {
                panic!("Got method that is not closure!")
            }
        } else {
            let msg = format!("Undefined property '{}'.", name.deref());
            self.runtime_error(&msg)
        }
    }

    fn bind_method(&mut self, class: GcRef<Class>, name: GcRef<LoxString>) -> Result<(), LoxError> {
        if let Some(method) = class.methods.get(name) {
            let receiver = self.peek(0);
            let method = match method {
                Value::Closure(closure) => closure,
                _ => panic!("Inconsistent state. Method is not closure"),
            };
            let bound = BoundMethod::new(receiver, method);
            let bound = self.alloc(bound);
            self.pop();
            self.push(Value::BoundMethod(bound));
            Ok(())
        } else {
            let msg = format!("Undefined property '{}'.", name.deref());
            self.runtime_error(&msg)
        }
    }

    fn capture_upvalue(&mut self, location: usize) -> GcRef<Upvalue> {
        for &upvalue in &self.open_upvalues {
            if upvalue.location == location {
                return upvalue;
            }
        }
        let upvalue = Upvalue::new(location);
        let upvalue = self.alloc(upvalue);
        self.open_upvalues.push(upvalue);
        upvalue
    }
    fn close_upvalues(&mut self, last: usize) {
        let mut i = 0;
        while i != self.open_upvalues.len() {
            let mut upvalue = self.open_upvalues[i];
            if upvalue.location >= last {
                // PERF: Remove is expensive
                self.open_upvalues.remove(i);
                let location = upvalue.location;
                upvalue.closed = Some(self.stack[location]);
            } else {
                i += 1;
            }
        }
    }

    fn define_method(&mut self, name: GcRef<LoxString>) {
        let method = self.peek(0);
        if let Value::Class(mut class) = self.peek(1) {
            class.methods.set(name, method);
            self.pop();
        } else {
            panic!("Invalid state: trying to define a method of non class");
        }
    }

    fn alloc<T: GcObject + 'static>(&mut self, object: T) -> GcRef<T> {
        self.mark_and_sweep();
        self.gc.alloc(object)
    }

    fn intern(&mut self, name: String) -> GcRef<LoxString> {
        self.mark_and_sweep();
        self.gc.intern(name)
    }

    fn mark_and_sweep(&mut self) {
        if self.gc.should_gc() {
            #[cfg(feature = "debug_log_gc")]
            println!("-- gc begin");

            self.mark_roots();
            self.gc.collect_garbage();

            #[cfg(feature = "debug_log_gc")]
            println!("-- gc end");
        }
    }

    fn mark_roots(&mut self) {
        for &value in &self.stack[0..self.stack_len()] {
            self.gc.mark_value(value);
        }

        for frame in &self.frames[..self.frame_count] {
            self.gc.mark_object(frame.closure)
        }

        for &upvalue in &self.open_upvalues {
            self.gc.mark_object(upvalue);
        }

        self.gc.mark_table(&self.globals);
        self.gc.mark_object(self.init_string);
    }
}

#[derive(Clone, Copy)]
struct CallFrame {
    closure: GcRef<Closure>,
    ip: *const Instruction,
    slot: usize,
}

impl CallFrame {
    fn new(closure: GcRef<Closure>, slot: usize) -> Self {
        CallFrame {
            closure,
            ip: closure.function.chunk.code.as_ptr(),
            slot,
        }
    }

    fn offset(&self) -> usize {
        unsafe {
            let chunk = &self.closure.function.chunk;
            let pos = self.ip.offset_from(chunk.code.as_ptr());
            pos as usize
        }
    }

    fn line(&self) -> usize {
        self.closure.function.chunk.lines[self.offset() - 1]
    }
}

fn clock(vm: &Vm, _args: &[Value]) -> Value {
    let time = vm.start_time.elapsed().as_secs_f64();
    Value::Number(time)
}

fn lox_panic(_vm: &Vm, args: &[Value]) -> Value {
    let mut terms: Vec<String> = vec![];

    for &arg in args.iter() {
        let term = format!("{}", arg);
        terms.push(term);
    }

    panic!("panic: {}", terms.join(", "))
}
