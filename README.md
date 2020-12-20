# jlrs

[![Build Status](https://travis-ci.com/Taaitaaiger/jlrs.svg?branch=master)](https://travis-ci.com/Taaitaaiger/jlrs)
[![Windows Build Status](https://ci.appveyor.com/api/projects/status/github/taaitaaiger/jlrs?branch=master&svg=true)](https://ci.appveyor.com/project/Taaitaaiger/jlrs?branch=master)
[![Coverage Status](https://coveralls.io/repos/github/Taaitaaiger/jlrs/badge.svg?branch=master)](https://coveralls.io/github/Taaitaaiger/jlrs?branch=master)
[![Rust Docs](https://docs.rs/jlrs/badge.svg)](https://docs.rs/jlrs)
[![License:MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)


The main goal behind jlrs is to provide a simple and safe interface to the Julia C API that
lets you call code written in Julia from Rust and vice versa. Currently this crate is only
tested on Linux and Windows in combination with Julia 1.5 and is not compatible with earlier
versions of Julia.


## Features

An incomplete list of features that are currently supported by jlrs:

 - Access arbitrary Julia modules and their contents.
 - Call arbitrary Julia functions, including functions that take keyword arguments.
 - Include and use your own Julia code.
 - Load a custom system image.
 - Create values that Julia can use, and convert them back to Rust, from Rust.
 - Access the type information and fields of values and check their properties.
 - Create and use n-dimensional arrays.
 - Support for mapping Julia structs to Rust structs which can be generated with `JlrsReflect.jl`.
 - Structs that can be mapped to Rust include those with type parameters and bits unions.
 - Use these features when calling Rust from Julia through `ccall`.
 - Offload long-running functions to another thread and `.await` the result with the (experimental) async runtime.


## Usage

Add this to your `Cargo.toml`:

```toml
[dependencies]
jlrs = "0.8"
```

This crate depends on `jl-sys` which contains the raw bindings to the Julia C API, these are generated by `bindgen`. You can find the requirements for using `bindgen` in [their User Guide](https://rust-lang.github.io/rust-bindgen/requirements.html).

#### Linux

The recommended way to install Julia is to download the binaries from the official website,
which is distributed in an archive containing a directory called `julia-x.y.z`. This directory
contains several other directories, including a `bin` directory containing the `julia`
executable.

In order to ensure the `julia.h` header file can be found, either `/usr/include/julia/julia.h`
must exist, or you have to set the `JULIA_DIR` environment variable to `/path/to/julia-x.y.z`.
This environment variable can be used to override the default. Similarly, in order to load
`libjulia.so` you must add `/path/to/julia-x.y.z/lib` to the `LD_LIBRARY_PATH` environment
variable.

#### Windows

The recommended way to install Julia is to download the installer from the official website,
which will install Julia in a folder called `Julia-x.y.z`. This folder contains several other
folders, including a `bin` folder containing the `julia.exe` executable. You must set the
`JULIA_DIR` environment variable to the `Julia-x.y.z` folder and add `Julia-x.y.z\bin` to the
`PATH` environment variable. For example, if Julia is installed at `D:\Julia-x.y.z`,
`JULIA_DIR` must be set to `D:\Julia-x.y.z` and `D:\Julia-x.y.z\bin` must be added to `PATH`.

Additionally, MinGW must be installed through Cygwin. To install this and all potentially
required dependencies, follow steps 1-4 of
[the instructions for compiling Julia on Windows using Cygwin and MinGW](https://github.com/JuliaLang/julia/blob/v1.4.1/doc/build/windows.md#cygwin-to-mingw-cross-compiling).
You must set the `CYGWIN_DIR` environment variable to the installation folder of Cygwin; this
folder contains some icons, `Cygwin.bat` and folders with names like `usr` and `bin`. For
example, if Cygwin is installed at `D:\cygwin64`, `CYGWIN_DIR` must be set to `D:\cygwin64`.

Julia is compatible with the GNU toolchain on Windows. If you use rustup, you can set the
toolchain for a project that depends on `jl-sys` by calling the command
`rustup override set stable-gnu` in the project root folder.


## Interacting with Julia

The first thing you should do is `use` the `prelude`-module with an asterisk, this will
bring all the structs and traits you're likely to need into scope. If you're calling Julia
from Rust, you must initialize Julia before you can use it. You can do this by calling
`Julia::init`. Note that this method can only be called once, if you drop `Julia` you won't
be able to create a new one and have to restart the entire program. If you want to use a
custom system image, you must call `Julia::init_with_image` instead of `Julia::init`.
If you're calling Rust from Julia everything has already been initialized, you can use `CCall`
instead.

#### Calling Julia from Rust

You can call `Julia::include` to include your own Julia code and either `Julia::frame` or
`Julia::dynamic_frame` to interact with Julia.

The other two methods, `Julia::frame` and `Julia::dynamic_frame`, take a closure that
provides you with a `Global`, and either a `StaticFrame` or `DynamicFrame` respectively.
`Global` is a token that lets you access Julia modules their contents, and other global
values, while the frames are used to deal with local Julia data.

Local data must be handled properly: Julia is a programming language with a garbage collector
that is unaware of any references to data outside of Julia. In order to make it aware of this
usage a stack must be maintained. You choose this stack's size when calling `Julia::init`.
The elements of this stack are called stack frames; they contain a pointer to the previous
frame, the number of protected values, and that number of pointers to values. The two frame
types offered by jlrs take care of all the technical details, a `DynamicFrame` will grow
to the required size while a `StaticFrame` has a definite number of slots. These frames can
be nested (ie stacked) arbitrarily.

In order to call a Julia function, you'll need two things: a function to call, and arguments
to call it with. You can acquire the function through the module that defines it with
`Module::function`; `Module::base` and `Module::core` provide access to Julia's `Base`
and `Core` module respectively, while everything you include through `Julia::include` is
made available relative to the `Main` module which you can access by calling `Module::main`.

Julia data is represented by a `Value`. Basic data types like numbers, booleans, and strings
can be created through `Value::new` and several methods exist to create an n-dimensional
array. Each value will be protected by a frame, and the two share a lifetime in order to
enforce that a value can only be used as long as its protecting frame hasn't been dropped.
Julia functions, their arguments and their results are all `Value`s too. All `Value`s can be
called as functions, whether this will succeed depends on the value actually being a function.
You can copy data from Julia to Rust by calling `Value::cast`.

As a simple example, let's create two values and add them:

```rust
use jlrs::prelude::*;

fn main() {
    let mut julia = unsafe { Julia::init(16).unwrap() };
    julia.dynamic_frame(|global, frame| {
        // Create the two arguments
        let i = Value::new(frame, 2u64)?;
        let j = Value::new(frame, 1u32)?;

        // We can find the addition-function in the base module
        let func = Module::base(global).function("+")?;

        // Call the function and unbox the result
        let output = func.call2(frame, i, j)?.unwrap();
        output.cast::<u64>()
    }).unwrap();
}
```

You can also do this with a static frame:

```rust
use jlrs::prelude::*;

fn main() {
    let mut julia = unsafe { Julia::init(16).unwrap() };
    // Three slots; two for the inputs and one for the output.
    julia.frame(3, |global, frame| {
        // Create the two arguments, each value requires one slot
        let i = Value::new(frame, 2u64)?;
        let j = Value::new(frame, 1u32)?;

        // We can find the addition-function in the base module
        let func = Module::base(global).function("+")?;

        // Call the function and unbox the result.  
        let output = func.call2(frame, i, j)?.unwrap();
        output.cast::<u64>()
    }).unwrap();
}
```

This is only a small example, other things can be done with `Value` as well: their fields
can be accessed if the `Value` is some tuple or struct. They can contain more complex data;
if a function returns an array or a module it will still be returned as a `Value`. There
complex types are compatible with `Value::cast`. Additionally, you can create `Output`s in
a frame in order to protect a value from with a specific frame; this value will share that
frame's lifetime.

#### Standard library and installed packages
Julia has a standard library that includes modules like `LinearAlgebra` and `Dates`, and comes
with a package manager that makes it easy to install new packages. In order to use these 
modules and packages, they must first be loaded. This can be done by calling `Module::require`.

#### Calling Rust from Julia

Julia's `ccall` interface can be used to call `extern "C"` functions defined in Rust. There
are two major ways to use `ccall`, with a pointer to the function or a
`(:function, "library")` pair.

A function can be cast to a void pointer and converted to a `Value`:

```rust
use jlrs::prelude::*;

unsafe extern "C" fn call_me(arg: bool) -> isize {
    if arg {
        1
    } else {
        -1
    }
}

fn main() {
    let mut julia = unsafe { Julia::init(16).unwrap() };
    julia.frame(2, |global, frame| {
        // Cast the function to a void pointer
        let call_me_val = Value::new(frame, call_me as *mut std::ffi::c_void)?;

        // `myfunc` will call the function pointer, it's defined in the next block of code
        let func = Module::main(global).function("myfunc")?;

        // Call the function and unbox the result.  
        let output = func.call1(frame, call_me_val)?
            .unwrap()
            .cast::<isize>()?;

        assert_eq!(output, 1);
        
        Ok(())
    }).unwrap();
}
```

This pointer can be called from Julia:

```julia
function myfunc(callme::Ptr{Cvoid})::Int
    ccall(callme, Int, (Bool,), true)
end
```

You can also use functions defined in `dylib` and `cdylib` libraries. In order to create such
a library you need to add

```toml
[lib]
crate-type = ["dylib"]
```

or  

```toml
[lib]
crate-type = ["cdylib"]
```

respectively to your crate's `Cargo.toml`. Use a `dylib` if you want to use the crate in other
Rust crates, but if it's only intended to be called through `ccall` a `cdylib` is the better
choice. On Linux, compiling such a crate will be compiled to `lib<crate_name>.so`, on Windows
`lib<crate_name>.dll`.

The functions you want to use with `ccall` must be both `extern "C"` functions to ensure the C
ABI is used, and annotated with `#[no_mangle]` to prevent name mangling. Julia can find
libraries in directories that are either on the default library search path or included by
setting the `LD_LIBRARY_PATH` environment variable on Linux, or `PATH` on Windows. If the
compiled library is not directly visible to Julia, you can open it with `Libdl.dlopen` and
acquire function pointers with `Libdl.dlsym`. These pointers can be called the same way as
the pointer in the previous example.

If the library is visible to Julia you can access it with the library name. If `call_me` is
defined in a crate called `foo`, the following should work:

```julia
ccall((:call_me, "libfoo"), Int, (Bool,), false)
```

One important aspect of calling Rust from other languages in general is that panicking across
an FFI boundary is undefined behaviour. If you're not sure your code will never panic, wrap it
with `std::panic::catch_unwind`.

Many features provided by jlrs including accessing modules, calling functions, and borrowing
array data require a `Global` or a frame. You can access these by creating a `CCall`
first.


#### Async runtime

The experimental async runtime runs Julia in a separate thread and allows multiple tasks to
run in parallel by offloading functions to a new thread in Julia and waiting for them to
complete without blocking the runtime. To use this feature you must to enable the `async`
feature flag:

```toml
[dependencies]
jlrs = { version = "0.8", features = ["async"] }
```

This features is only supported on Linux.

The struct `AsyncJulia` is exported by the prelude and lets you initialize the runtime in
two ways, either as a task or as a thread. The first type should be used if you want to
integrate the async runtime into a larger project that uses `async_std`. In order for the
runtime to work correctly the `JULIA_NUM_THREADS` environment variable must be set to a value
larger than 1.

In order to call Julia with the async runtime you must implement the `JuliaTask` trait. The
`run`-method of this trait is similar to the closures that are used in the examples
above for the sync runtime; it provides you with a `Global` and an `AsyncFrame` which
implements the `Frame` trait. The `AsyncFrame` is required to use `Value::call_async`
which calls a function on a new thread using `Base.Threads.@spawn` and returns a `Future`.
While you await the result the runtime can handle another task. If you don't use
`Value::call_async` tasks are handled sequentially.

It's important to keep in mind that allocating memory in Julia uses a lock, so if you run
multiple functions at the same time that allocate new values frequently the performance will
drop significantly. The garbage collector can only run when all threads have reached a
safepoint, which is the case whenever a function needs to allocate memory. If your function
takes a long time to complete but needs to allocate rarely, you should periodically call
`GC.safepoint` in Julia to ensure the garbage collector can run.

You can find fully commented basic examples in [the examples directory of the repo].


# Custom types

In order to map a struct in Rust to one in Julia you can derive `JuliaStruct`. This will
implement `Cast`, `JuliaType`, `ValidLayout`, and `JuliaTypecheck` for that type. If
the struct in Julia has no type parameters and is a bits type you can also derive
`IntoJulia`, which lets you use the type in combination with `Value::new`.

You should not implement these structs manually. The `JlrsReflect.jl` package can generate
generate the correct Rust struct for types that don't include any unions or tuples with type
parameters. The reason for this restriction is that the layout of tuple and union fields can
be very different depending on these parameters in a way that can't be nicely expressed in
Rust.

These custom types can also be used when you call Rust from Julia through `ccall`.
