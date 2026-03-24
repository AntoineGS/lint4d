unit MainForm;

interface

uses
  Winapi.Windows, Winapi.Messages, System.SysUtils, System.Variants, System.Classes, Vcl.Graphics,
  Vcl.Controls, Vcl.Forms, Vcl.Dialogs;

type
  TForm1 = class(TForm)
    procedure FormCreate(Sender: TObject);
  public
    destructor Destroy; override;
  private
    FObj: TObject;
    procedure TestRefCounted;
    procedure TestNilBeforeFree;
    procedure TestNoFreeButHasTryFinally;
    procedure TestUseAfterFree;
    procedure TestDoubleFree;
    procedure TestCreateObjectInSubFunction;
    procedure TestObjInSubFunctionMayLeak;
    procedure TestFreeAfterCreate;
    // I wonder if it is possible to catch something like items added to TList, 
      // and TList does not free them
    // TODO: Add defensive coding pattern checks (no use of passed variables without a 
      // null check)
    // Constructor/Destructors should include FormCreate and FormDestroy
  end;

var
  Form1: TForm1;

implementation

uses
  interfaces;

{$R *.dfm}

function CreateObject: TObject;
begin
  result := TObject.Create; // should not warn for leak
end;

function CreateBadObject: TObject;
begin
  result := TObject.Create;

  if result.ClassName <> 'somestring' then // should warn for for leak, needs try..except..free
    raise Exception.Create('test');
end;

procedure TForm1.FormCreate(Sender: TObject);
begin
  FObj := TObject.Create; // should warn as memory leak
  TestRefCounted;
  TestNilBeforeFree;
  TestNoFreeButHasTryFinally;
  TestUseAfterFree;
  TestDoubleFree;
  TestCreateObjectInSubFunction;
  TestObjInSubFunctionMayLeak;
  TestFreeAfterCreate;
  inherited; // should warn as inherited should typically be at the top in Create
end;

destructor TForm1.Destroy;
begin
  inherited;

  if FObj.ClassName = 'TObject' then // should warn for running code after inherited in Destroy
    Exit
  else
    Raise Exception.Create('uhoh'); // should warn as raising in a destructor is bad practice
end;

procedure TForm1.TestFreeAfterCreate;
var
  aObj: TObject;
begin
  aObj := TObject.Create; // should not warn for leak
  aObj.Free;
end;

procedure TForm1.TestObjInSubFunctionMayLeak;
var
  aObj: TObject;
begin
  aObj := CreateBadObject; // should warn for leak inside this sub-function
  try
  finally
    aObj.Free;
  end;
end;

procedure TForm1.TestCreateObjectInSubFunction;
var
  aObj: TObject;
begin
  aObj := CreateObject; // should warn for leak

  if aObj.ClassName <> 'TObject' then
    raise Exception.Create('some message');
end;

procedure TForm1.TestDoubleFree;
var
  aObj: TObject;
begin
  aObj := TObject.Create;
  try
  finally
    aObj.Free;
    aObj.Free; // should warn Use after Free
  end;
end;

procedure TForm1.TestNoFreeButHasTryFinally;
var
  aObj: TObject;
begin
  aObj := TObject.Create; // should warn for leak
  try
  finally
  end;

  if aObj.ClassName <> 'TObject' then
    raise Exception.Create('sommessage');
end;

procedure TForm1.TestNilBeforeFree;
var
  aObj: TObject;
begin
  aObj := nil;
  try
    aObj := TObject.Create;  // should not warn for leak

    if aObj.ClassName <> 'TObject' then
      raise Exception.Create('somemessage');
  finally
    aObj.Free;
  end;
end;

procedure TForm1.TestRefCounted;
var
  aRefCountedObj: TRefCountedObject;
  aRefCountedItf: IInterface;
begin
  aRefCountedObj := TRefCountedObject.Create; // should not trigger warning for leak
  aRefCountedItf := aRefCountedObj;

  if aRefCountedObj.RefCount = 1 then
    raise Exception.Create('test');
end;

procedure TForm1.TestUseAfterFree;
var
  aObj: TObject;
begin
  aObj := nil;
  try
    aObj := TObject.Create;
  finally
    aObj.Free;
  end;

  if aObj.ClassName = 'NOTACLASS' then // should warn for Use After Free
    raise Exception.Create('Error Message');

  aObj := TObject.Create;
  try
  finally
    FreeAndNil(aObj);
  end;

  if aObj.ClassName = 'NOTACLASS' then // should warn for Use After Free
    raise Exception.Create('Error Message');
end;

end.
